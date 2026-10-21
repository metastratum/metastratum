<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Schemas, type Tenant } from '../../api/client'
import { useScope } from '../../stores/scope'

const props = defineProps<{ tenant: Tenant; parent?: Tenant }>()
const scope = useScope()

type Usable = { id: string; name: string; kind?: string }
type Offer = { clusters: Usable[]; pools: Usable[]; externals: Usable[]; repos: Usable[] }
const offered = ref<Offer | null>(null)
const usable = ref<Offer | null>(null)
const picked = ref<Required<Schemas['Grants']>>({
  clusters: [],
  storage_pools: [],
  external_networks: [],
  backup_repositories: [],
})
const error = ref('')
const saved = ref(false)
const busy = ref(false)

const editable = computed(() => !!props.parent && scope.can('grant.write', props.parent.id))

async function usableAt(id: string) {
  const [clusters, pools, externals, repos] = await Promise.all([
    ok(api.GET('/api/v1/tenants/{id}/clusters', { params: { path: { id } } })),
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id } } })),
    ok(api.GET('/api/v1/tenants/{id}/external-networks', { params: { path: { id } } })),
    ok(api.GET('/api/v1/tenants/{id}/backup-repositories', { params: { path: { id } } })),
  ])
  // Local storage is on every host and needs no grant.
  return { clusters, pools: pools.filter((p) => p.kind !== 'local'), externals, repos }
}

onMounted(async () => {
  try {
    const id = props.tenant.id
    usable.value = await usableAt(id)
    if (props.parent && editable.value) {
      ;[offered.value, picked.value] = await Promise.all([
        usableAt(props.parent.id),
        ok(api.GET('/api/v1/tenants/{id}/grants', { params: { path: { id } } })).then((g) => ({
          ...g,
          external_networks: g.external_networks ?? [],
          backup_repositories: g.backup_repositories ?? [],
        })),
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
    const g = await ok(
      api.PUT('/api/v1/tenants/{id}/grants', { params: { path: { id: props.tenant.id } }, body: picked.value }),
    )
    picked.value = {
      ...g,
      external_networks: g.external_networks ?? [],
      backup_repositories: g.backup_repositories ?? [],
    }
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
        Where {{ tenant.name }} can run VMs and keep their disks and backups. A tenant can only be given what
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
      <fieldset>
        <legend class="label">External networks</legend>
        <label v-for="e in offered.externals" :key="e.id" class="check">
          <input v-model="picked.external_networks" type="checkbox" :value="e.id" />
          {{ e.name }}
        </label>
        <p v-if="!offered.externals.length" class="muted">{{ parent.name }} has no external networks to give.</p>
        <p class="muted hint">For routing VPCs out, and floating IPs.</p>
      </fieldset>
      <fieldset>
        <legend class="label">Backup repositories</legend>
        <label v-for="r in offered.repos" :key="r.id" class="check">
          <input v-model="picked.backup_repositories" type="checkbox" :value="r.id" />
          {{ r.name }}
        </label>
        <p v-if="!offered.repos.length" class="muted">{{ parent.name }} has no backup repositories to give.</p>
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
      <div>
        <dt class="label">External networks</dt>
        <dd>
          <span v-for="e in usable.externals" :key="e.id" class="item">{{ e.name }}</span>
          <span v-if="!usable.externals.length" class="muted">None — VMs reach only each other.</span>
        </dd>
      </div>
      <div>
        <dt class="label">Backup repositories</dt>
        <dd>
          <span v-for="r in usable.repos" :key="r.id" class="item">{{ r.name }}</span>
          <span v-if="!usable.repos.length" class="muted">None — VMs can’t be backed up.</span>
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
  grid-template-columns: repeat(auto-fit, minmax(min(220px, 100%), 1fr));
  gap: 16px;
}
fieldset {
  display: grid;
  gap: 8px;
  align-content: start;
  margin: 0;
  padding: 12px 14px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
legend {
  padding: 0 6px;
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
/* What the tenant has, a row per kind. */
.read {
  display: grid;
  gap: 10px;
  margin: 0;
}
.read > div {
  display: grid;
  grid-template-columns: 180px minmax(0, 1fr);
  align-items: center;
  gap: 16px;
}
.read dd {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  margin: 0;
}
@media (max-width: 560px) {
  .read > div {
    grid-template-columns: minmax(0, 1fr);
    gap: 4px;
  }
}
.item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
</style>
