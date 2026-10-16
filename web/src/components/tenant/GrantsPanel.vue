<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Schemas, type Tenant } from '../../api/client'
import { useScope } from '../../stores/scope'

const props = defineProps<{ tenant: Tenant; parent?: Tenant }>()
const scope = useScope()

type Usable = { id: string; name: string; kind?: string }
const offered = ref<{ clusters: Usable[]; pools: Usable[] } | null>(null)
const usable = ref<{ clusters: Usable[]; pools: Usable[] } | null>(null)
const picked = ref<Schemas['Grants']>({ clusters: [], storage_pools: [] })
const error = ref('')
const saved = ref(false)
const busy = ref(false)

const editable = computed(() => !!props.parent && scope.can('grant.write', props.parent.id))

async function usableAt(id: string) {
  const [clusters, pools] = await Promise.all([
    ok(api.GET('/api/v1/tenants/{id}/clusters', { params: { path: { id } } })),
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id } } })),
  ])
  // Local storage is on every host and needs no grant.
  return { clusters, pools: pools.filter((p) => p.kind !== 'local') }
}

onMounted(async () => {
  try {
    const id = props.tenant.id
    usable.value = await usableAt(id)
    if (props.parent && editable.value) {
      ;[offered.value, picked.value] = await Promise.all([
        usableAt(props.parent.id),
        ok(api.GET('/api/v1/tenants/{id}/grants', { params: { path: { id } } })),
      ])
    }
  } catch (e) {
    error.value = errorText(e)
  }
})

async function save() {
  error.value = ''
  saved.value = false
  busy.value = true
  try {
    picked.value = await ok(
      api.PUT('/api/v1/tenants/{id}/grants', { params: { path: { id: props.tenant.id } }, body: picked.value }),
    )
    usable.value = await usableAt(props.tenant.id)
    saved.value = true
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="panel">
    <p class="muted lead">
      <template v-if="!tenant.parent_id">The provider can use every cluster and storage pool.</template>
      <template v-else>
        Where {{ tenant.name }} can run VMs and keep their disks. A tenant can only be given what
        {{ parent?.name ?? 'the tenant above' }} has, and taking something away also takes it from the tenants
        below.
      </template>
    </p>

    <form v-if="parent && editable && offered" class="grants" @submit.prevent="save">
      <fieldset>
        <legend class="label">Clusters</legend>
        <label v-for="c in offered.clusters" :key="c.id" class="check">
          <input v-model="picked.clusters" type="checkbox" :value="c.id" />
          {{ c.name }}
        </label>
        <p v-if="!offered.clusters.length" class="muted">{{ parent.name }} has no clusters to give.</p>
      </fieldset>
      <fieldset>
        <legend class="label">Shared storage</legend>
        <label v-for="p in offered.pools" :key="p.id" class="check">
          <input v-model="picked.storage_pools" type="checkbox" :value="p.id" />
          {{ p.name }} <span class="tag">{{ p.kind }}</span>
        </label>
        <p v-if="!offered.pools.length" class="muted">{{ parent.name }} has no shared storage to give.</p>
        <p class="muted hint">Local disks on each host are always available.</p>
      </fieldset>
      <footer>
        <span v-if="saved" class="muted" role="status">Saved.</span>
        <button class="btn primary" :disabled="busy">Save</button>
      </footer>
    </form>

    <dl v-else-if="usable" class="read">
      <div>
        <dt class="label">Clusters</dt>
        <dd>
          <span v-for="c in usable.clusters" :key="c.id" class="item">{{ c.name }}</span>
          <span v-if="!usable.clusters.length" class="muted">None — VMs can’t be placed yet.</span>
        </dd>
      </div>
      <div>
        <dt class="label">Shared storage</dt>
        <dd>
          <span v-for="p in usable.pools" :key="p.id" class="item">{{ p.name }} <span class="tag">{{ p.kind }}</span></span>
          <span v-if="!usable.pools.length" class="muted">Local disks only.</span>
        </dd>
      </div>
    </dl>

    <p v-if="error" class="error-text" role="alert">{{ error }}</p>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 18px;
}
.lead {
  max-width: 72ch;
  margin: 0;
}
.grants {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: 20px;
}
fieldset {
  display: grid;
  gap: 8px;
  align-content: start;
  margin: 0;
  padding: 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
legend {
  padding: 0 4px;
}
fieldset p {
  margin: 0;
}
.hint {
  font-size: 12px;
}
footer {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 12px;
  grid-column: 1 / -1;
}
.read {
  display: grid;
  gap: 14px;
  margin: 0;
}
.read dd {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  margin: 6px 0 0;
}
.item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
</style>
