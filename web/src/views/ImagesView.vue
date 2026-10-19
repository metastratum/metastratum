<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, ApiError, ok, type Image, type Schemas } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { ago, gib } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
const images = ref<Image[] | null>(null)
const adding = ref(false)
const LOCAL_POOL = '00000000-0000-0000-0000-000000000001'
const blank = () => ({
  name: '',
  source_url: '',
  format: 'qcow2',
  os: 'linux',
  shared: false,
  secure_boot: false,
  tpm: false,
  storage_pool_id: LOCAL_POOL,
})
const form = ref(blank())
// Where images can be kept: the pools the tenant may use.
const pools = ref<Schemas['UsablePool'][]>([])
const error = ref('')

async function load() {
  if (!scope.current) return
  images.value = await ok(api.GET('/api/v1/images', { params: { query: { tenant_id: scope.current } } }))
}
watch(() => scope.current, load, { immediate: true })
async function openAdd() {
  adding.value = true
  pools.value = await ok(
    api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id: scope.current } } }),
  ).catch(() => [])
}
useLiveRefresh(['image'], load)

async function add() {
  error.value = ''
  try {
    await ok(api.POST('/api/v1/images', { body: { ...form.value, tenant_id: scope.current } }))
    adding.value = false
    form.value = blank()
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : 'The controller isn’t responding. Try again.'
  }
}

// Deleting, after a confirmation that says what goes.
const deleting = ref<Image | null>(null)
const deleteError = ref('')
async function remove() {
  const img = deleting.value
  if (!img) return
  deleteError.value = ''
  try {
    await ok(api.DELETE('/api/v1/images/{id}', { params: { path: { id: img.id } } }))
    deleting.value = null
    await load()
  } catch (e) {
    deleteError.value = e instanceof ApiError ? e.message : 'Could not delete the image. Try again.'
  }
}

// Downloading it again, after that failed, or stopped without saying how.
const retryable = (img: Image) => img.state === 'failed' || img.stuck
const retrying = ref('')
const retryError = ref<Record<string, string>>({})
async function downloadAgain(img: Image) {
  retrying.value = img.id
  retryError.value = { ...retryError.value, [img.id]: '' }
  try {
    await ok(api.POST('/api/v1/images/{id}/download', { params: { path: { id: img.id } } }))
    await load()
  } catch (e) {
    const message = e instanceof ApiError ? e.message : 'The controller isn’t responding. Try again.'
    retryError.value = { ...retryError.value, [img.id]: message }
  } finally {
    retrying.value = ''
  }
}

const own = (img: Image) => img.tenant_id === scope.current
</script>

<template>
  <section>
    <header class="head">
      <h1>Images</h1>
      <button v-if="scope.can('image.write')" class="btn primary" @click="openAdd">Add image</button>
    </header>
    <p class="intro">
      A host downloads an image as you add it. Hosts copy it from there to make VMs from it, and keep their copy
      for the next. A VM’s disk is a copy of its own, so it doesn’t depend on the image: VMs keep working when the
      image is gone.
    </p>

    <div v-if="images && !images.length" class="empty">
      <p>Add an image so VMs have something to boot. Any cloud image in qcow2 or raw format works.</p>
      <button v-if="scope.can('image.write')" class="btn primary" @click="openAdd">Add image</button>
    </div>

    <ul v-else-if="images" class="list">
      <li v-for="img in images" :key="img.id">
        <div class="main">
          <strong>{{ img.name }}</strong>
          <span class="tag label">{{ img.format }}</span>
          <span v-if="img.os === 'windows'" class="tag label">Windows</span>
          <span v-if="img.secure_boot" class="tag label">Secure Boot</span>
          <span v-if="img.tpm" class="tag label">TPM</span>
          <span v-if="img.shared && !scope.solo" class="tag label">Shared with sub-tenants</span>
          <span v-if="!own(img)" class="tag label">From parent tenant</span>
          <span v-if="img.stuck" class="tag label failed">Download stopped</span>
          <span v-else-if="img.state === 'downloading'" class="tag label busy">Downloading…</span>
          <span v-else-if="img.state === 'failed'" class="tag label failed">Download failed</span>
        </div>
        <div class="url mono">{{ img.source_url }}</div>
        <p v-if="img.state === 'failed'" class="error-text" role="alert">{{ img.error ?? 'The download failed.' }}</p>
        <p v-if="img.stuck" class="error-text" role="alert">The download stopped without finishing. Download it again.</p>
        <p v-if="retryError[img.id]" class="error-text" role="alert">{{ retryError[img.id] }}</p>
        <div v-if="img.state === 'ready' && img.virtual_bytes" class="facts muted">
          <span>{{ gib(img.virtual_bytes) }} disk</span>
          <span v-if="img.size_bytes">{{ gib(img.size_bytes) }} stored</span>
          <span v-if="img.storage_pool_id !== LOCAL_POOL">Kept in {{ img.storage_pool_name }}</span>
          <span v-if="img.hosts.length">On {{ img.hosts.join(', ') }}</span>
        </div>
        <div class="meta">
          <IdChip :value="img.id" short />
          <span class="muted">Added {{ ago(img.created_at) }}</span>
          <button
            v-if="retryable(img) && own(img) && scope.can('image.write')"
            class="btn small push"
            :disabled="retrying === img.id"
            @click="downloadAgain(img)"
          >
            Download again
          </button>
          <button
            v-if="own(img) && scope.can('image.write')"
            class="btn danger small"
            :class="{ push: !(retryable(img) && own(img)) }"
            @click="(deleteError = ''), (deleting = img)"
          >
            Delete…
          </button>
        </div>
      </li>
    </ul>

    <ModalDialog v-if="deleting" title="Delete image" @close="deleting = null">
      <form class="body" @submit.prevent="remove">
        <p>Delete <strong>{{ deleting.name }}</strong>? This can’t be undone:</p>
        <ul class="consequences">
          <li>New VMs can no longer be created from it.</li>
          <li v-if="deleting.shared">Sub-tenants it is shared with lose it too.</li>
          <li class="error-text">Every host deletes its copy. Adding the image again means downloading it again.</li>
          <li>VMs made from it keep working: their disks are copies of their own.</li>
        </ul>
        <p v-if="deleteError" class="error-text" role="alert">{{ deleteError }}</p>
        <footer>
          <button type="button" class="btn" @click="deleting = null">Cancel</button>
          <button class="btn danger">Delete {{ deleting.name }}</button>
        </footer>
      </form>
    </ModalDialog>

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
        <label class="field">
          <span class="label">Operating system</span>
          <select v-model="form.os">
            <option value="linux">Linux (cloud-init)</option>
            <option value="windows">Windows (cloudbase-init)</option>
          </select>
          <span v-if="form.os === 'windows'" class="hint">
            An image built with <code>deploy/images/windows/build.sh</code>: VMs get their Administrator password at
            first boot, and run with Hyper-V enlightenments.
          </span>
        </label>
        <fieldset class="field boot">
          <legend class="label">VMs made from it get</legend>
          <label class="check"><input v-model="form.secure_boot" type="checkbox" /> UEFI Secure Boot</label>
          <label class="check"><input v-model="form.tpm" type="checkbox" /> TPM 2.0</label>
          <span class="hint">Windows 11 needs both. Each VM can be made without them.</span>
        </fieldset>
        <label v-if="pools.length > 1" class="field">
          <span class="label">Keep it in</span>
          <select v-model="form.storage_pool_id">
            <option v-for="p in pools" :key="p.id" :value="p.id">
              {{ p.kind === 'local' ? 'Local disk of a host' : `${p.name} (${p.kind})` }}
            </option>
          </select>
          <span class="hint">
            Hosts that reach a shared pool make VMs’ disks from it directly; others copy it to their local disk.
          </span>
        </label>
        <label v-if="!scope.solo" class="check">
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
.boot {
  margin: 0;
  padding: 0;
  border: 0;
}
.boot legend {
  margin-bottom: 4px;
}
.consequences {
  margin: 0;
  padding-left: 18px;
  display: grid;
  gap: 6px;
}
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
}
.push {
  margin-left: auto;
}
.facts {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
}
.busy {
  color: var(--muted);
}
.failed {
  color: var(--led-fault);
  border-color: currentColor;
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
