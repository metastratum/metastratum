<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, ApiError, ok, type Image, type Schemas, type Vm } from '../api/client'
import { useScope } from '../stores/scope'
import ModalDialog from './ModalDialog.vue'

const emit = defineEmits<{ close: []; created: [vm: Vm] }>()

const scope = useScope()
const tenantId = scope.current
const images = ref<Image[]>([])
const clusters = ref<Schemas['UsableCluster'][]>([])
const pools = ref<Schemas['UsablePool'][]>([])
const clusterId = ref<string | null>(null)
const poolId = ref<string | null>(null)
const loaded = ref(false)
const name = ref('')
const imageId = ref('')
const vcpus = ref(2)
const memory = ref(2048)
const disk = ref(10)
const sshKey = ref('')
const password = ref('')
const error = ref('')
const busy = ref(false)

const MEMORY = [512, 1024, 2048, 4096, 8192, 16384]
const nameOk = computed(() => /^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(name.value))

onMounted(async () => {
  const path = { params: { path: { id: tenantId } } }
  ;[images.value, clusters.value, pools.value] = await Promise.all([
    ok(api.GET('/api/v1/images', { params: { query: { tenant_id: tenantId } } })),
    ok(api.GET('/api/v1/tenants/{id}/clusters', path)),
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', path)),
  ])
  imageId.value = images.value[0]?.id ?? ''
  poolId.value = pools.value.find((p) => p.kind === 'local')?.id ?? pools.value[0]?.id ?? null
  loaded.value = true
})

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const res = await ok(
      api.POST('/api/v1/vms', {
        body: {
          tenant_id: tenantId,
          cluster_id: clusterId.value,
          storage_pool_id: poolId.value,
          name: name.value,
          image_id: imageId.value,
          vcpus: vcpus.value,
          memory_mib: memory.value,
          disk_gib: disk.value,
          ssh_authorized_keys: sshKey.value.split('\n').map((k) => k.trim()).filter(Boolean),
          password: password.value || null,
        },
      }),
    )
    emit('created', res as unknown as Vm)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : 'The controller isn’t responding. Try again.'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Create VM in ${scope.tenant?.name ?? 'tenant'}`" @close="emit('close')">
    <p v-if="loaded && !images.length" class="body empty">
      There are no images to boot from yet.
      <RouterLink to="/images" @click="emit('close')">Add an image</RouterLink> first.
    </p>
    <p v-else-if="loaded && !clusters.length" class="body empty">
      {{ scope.tenant?.name }} hasn’t been given a cluster to run VMs on. Ask an administrator of the tenant above
      it.
    </p>
    <form v-else class="body" @submit.prevent="submit">
      <label class="field">
        <span class="label">Name</span>
        <input v-model="name" required maxlength="63" placeholder="web-1" autofocus />
        <span class="hint" :class="{ 'error-text': name && !nameOk }">
          Letters, digits and hyphens. Also used as the guest’s hostname.
        </span>
      </label>
      <label class="field">
        <span class="label">Image</span>
        <select v-model="imageId" required>
          <option v-for="i in images" :key="i.id" :value="i.id">{{ i.name }}</option>
        </select>
      </label>
      <div v-if="clusters.length > 1 || pools.length > 1" class="row two">
        <label class="field">
          <span class="label">Cluster</span>
          <select v-model="clusterId">
            <option :value="null">Any with room</option>
            <option v-for="c in clusters" :key="c.id" :value="c.id">{{ c.name }}</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Disk storage</span>
          <select v-model="poolId">
            <option v-for="p in pools" :key="p.id" :value="p.id">
              {{ p.kind === 'local' ? 'Local disk on the host' : `${p.name} (${p.kind})` }}
            </option>
          </select>
        </label>
      </div>
      <div class="row">
        <label class="field">
          <span class="label">vCPUs</span>
          <select v-model.number="vcpus">
            <option v-for="n in [1, 2, 4, 8, 16]" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Memory</span>
          <select v-model.number="memory">
            <option v-for="m in MEMORY" :key="m" :value="m">{{ m >= 1024 ? `${m / 1024} GiB` : `${m} MiB` }}</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Disk (GiB)</span>
          <input v-model.number="disk" type="number" min="1" max="16384" required />
        </label>
      </div>
      <label class="field">
        <span class="label">SSH public keys</span>
        <textarea v-model="sshKey" rows="3" placeholder="ssh-ed25519 AAAA… one per line" />
      </label>
      <label class="field">
        <span class="label">Console password</span>
        <input v-model="password" type="password" autocomplete="new-password" />
        <span class="hint">Optional. Lets you sign in on the serial console as the image’s default user.</span>
      </label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || !nameOk || !imageId">
          {{ busy ? 'Creating…' : 'Create VM' }}
        </button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.body {
  display: grid;
  gap: 16px;
  margin: 0;
  padding: 18px;
}
.row {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 12px;
}
.row.two {
  grid-template-columns: 1fr 1fr;
}
footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 4px;
}
.empty {
  color: var(--muted);
}
.error-text {
  margin: 0;
}
@media (max-width: 520px) {
  .row {
    grid-template-columns: 1fr 1fr;
  }
}
</style>
