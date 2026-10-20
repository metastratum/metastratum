<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, ApiError, ok, type Image, type Schemas } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { ago, gib } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { detectFormat, dismissUpload, stopUpload, upload, uploads } from '../stores/uploads'

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
  sleep: true,
  admin_user: '',
  storage_pool_id: LOCAL_POOL,
})
const form = ref(blank())
// Downloaded from a URL, or uploaded from a file here.
const source = ref<'url' | 'file'>('url')
const file = ref<File | null>(null)
async function pickFile(e: Event) {
  const f = (e.target as HTMLInputElement).files?.[0] ?? null
  file.value = f
  if (!f) return
  if (!form.value.name) form.value.name = f.name.replace(/\.(qcow2|img|raw)$/i, '')
  form.value.format = await detectFormat(f)
}
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
    const admin_user = form.value.os === 'windows' ? form.value.admin_user.trim() || null : null
    const from = source.value === 'file' && file.value ? { upload_bytes: file.value.size } : { source_url: form.value.source_url }
    const body = { ...form.value, ...from, source_url: from.source_url ?? null, admin_user, tenant_id: scope.current }
    const created = await ok(api.POST('/api/v1/images', { body }))
    const picked = file.value
    adding.value = false
    form.value = blank()
    file.value = null
    source.value = 'url'
    await load()
    // In the background: the list shows how far it is.
    if (picked) void upload(created as unknown as Image, picked, 0, scope.current).then(load)
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

// Resuming an upload, from what arrived: the same file is picked again.
const resumeError = ref<Record<string, string>>({})
function resume(img: Image) {
  const input = document.createElement('input')
  input.type = 'file'
  input.onchange = () => {
    const f = input.files?.[0]
    if (!f) return
    if (f.size !== img.file_bytes) {
      resumeError.value = {
        ...resumeError.value,
        [img.id]: `That file is ${gib(f.size)}; the image's upload is ${gib(img.file_bytes ?? 0)}. Pick the same file.`,
      }
      return
    }
    resumeError.value = { ...resumeError.value, [img.id]: '' }
    dismissUpload(img.id)
    void upload(img, f, img.received_bytes ?? 0, scope.current).then(load)
  }
  input.click()
}
// How much of an image's file has come: this tab's upload, or what the
// controller says (a download's size may not be known).
const received = (img: Image) => uploads.get(img.id)?.sent ?? img.received_bytes ?? 0
const receivedPct = (img: Image) =>
  img.file_bytes ? Math.floor((100 * received(img)) / img.file_bytes) : undefined

const own = (img: Image) => img.tenant_id === scope.current
</script>

<template>
  <section>
    <header class="head">
      <h1>Images</h1>
      <button v-if="scope.can('image.write')" class="btn primary" @click="openAdd">Add image</button>
    </header>
    <p class="intro">
      An image comes to a host as you add it, downloaded from its URL or uploaded from a file: only then, never again
      later. Hosts copy it from there to make VMs from it, and keep their copy for the next. A VM’s disk is a copy of its own, so it doesn’t depend on the image: VMs keep working when the
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
          <span v-if="!img.sleep" class="tag label">No sleep</span>
          <span v-if="img.shared && !scope.solo" class="tag label">Shared with sub-tenants</span>
          <span v-if="!own(img)" class="tag label">From parent tenant</span>
          <span v-if="img.stuck" class="tag label failed">Storing stopped</span>
          <span v-else-if="img.state === 'uploading' && uploads.has(img.id) && !uploads.get(img.id)?.error" class="tag label busy">
            Uploading… {{ receivedPct(img) }}%
          </span>
          <span v-else-if="img.state === 'uploading'" class="tag label failed">Upload incomplete</span>
          <span v-else-if="img.state === 'downloading'" class="tag label busy">
            Downloading…<template v-if="receivedPct(img) !== undefined"> {{ receivedPct(img) }}%</template>
          </span>
          <span v-else-if="img.state === 'storing'" class="tag label busy">Checking and storing…</span>
          <span v-else-if="img.state === 'failed'" class="tag label failed">
            {{ img.origin === 'upload' ? 'Upload failed' : 'Download failed' }}
          </span>
        </div>
        <div v-if="img.file_bytes && img.state === 'ready'" class="url muted">
          {{ img.origin === 'upload' ? 'Uploaded' : 'Downloaded' }}, {{ gib(img.file_bytes) }}
        </div>
        <div v-if="img.state === 'uploading' || img.state === 'downloading'" class="upload">
          <progress v-if="receivedPct(img) !== undefined" :value="receivedPct(img)" max="100" />
          <span class="muted">
            {{ gib(received(img)) }}<template v-if="img.file_bytes"> of {{ gib(img.file_bytes) }}</template>
          </span>
        </div>
        <p v-if="uploads.get(img.id)?.error" class="error-text" role="alert">{{ uploads.get(img.id)?.error }}</p>
        <p v-if="resumeError[img.id]" class="error-text" role="alert">{{ resumeError[img.id] }}</p>
        <p v-if="img.state === 'failed'" class="error-text" role="alert">
          {{ img.error ?? 'Adding it failed.' }} Delete it and add it again.
        </p>
        <p v-if="img.stuck" class="error-text" role="alert">
          Adding it stopped without finishing. Delete it and add it again.
        </p>
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
            v-if="img.state === 'uploading' && uploads.has(img.id) && !uploads.get(img.id)?.error"
            class="btn small push"
            @click="stopUpload(img.id)"
          >
            Stop upload
          </button>
          <button
            v-else-if="img.state === 'uploading' && own(img) && scope.can('image.write')"
            class="btn small push"
            @click="resume(img)"
          >
            Resume upload…
          </button>
          <button
            v-if="own(img) && scope.can('image.write')"
            class="btn danger small"
            :class="{ push: img.state !== 'uploading' }"
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
          <li class="error-text">
            Every host deletes its copy. Adding the image again means
            {{ deleting.origin === 'upload' ? 'uploading' : 'downloading' }} it again.
          </li>
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
        <fieldset class="field boot">
          <legend class="label">From</legend>
          <label class="check"><input v-model="source" type="radio" value="url" /> A download URL</label>
          <label class="check"><input v-model="source" type="radio" value="file" /> A file on this computer</label>
        </fieldset>
        <label v-if="source === 'url'" class="field">
          <span class="label">Download URL</span>
          <input v-model="form.source_url" required type="url" placeholder="https://cloud.debian.org/…/debian-13-genericcloud-amd64.qcow2" />
        </label>
        <label v-else class="field">
          <span class="label">File</span>
          <input type="file" accept=".qcow2,.img,.raw" required @change="pickFile" />
          <span class="hint">
            Uploaded in the background once added; keep this page open until it is done. A stopped upload can be
            resumed from where it was.
          </span>
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
            An image built with <code>deploy/images/windows/build.sh</code>: VMs get their account and its password at
            first boot, and run with Hyper-V enlightenments.
          </span>
        </label>
        <label v-if="form.os === 'windows'" class="field">
          <span class="label">Default account</span>
          <input v-model="form.admin_user" maxlength="20" placeholder="Administrator" />
          <span class="hint">
            What VMs are signed in with unless they name another: Administrator on Windows Server. Leave it empty for
            Windows 11, which has none: each VM names its own.
          </span>
        </label>
        <fieldset class="field boot">
          <legend class="label">VMs made from it get</legend>
          <label class="check"><input v-model="form.secure_boot" type="checkbox" /> UEFI Secure Boot</label>
          <label class="check"><input v-model="form.tpm" type="checkbox" /> TPM 2.0</label>
          <label class="check"><input v-model="form.sleep" type="checkbox" /> Sleep when idle</label>
          <span class="hint">
            Windows 11 needs Secure Boot and a TPM. A VM asleep is woken by starting it. Each VM can be made otherwise.
          </span>
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
          <button class="btn primary">{{ source === 'file' ? 'Add and upload' : 'Add image' }}</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.upload {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 6px;
}
.upload progress {
  flex: 1;
  max-width: 320px;
}
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
