<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Tenant } from '../../api/client'
import { useScope } from '../../stores/scope'
import { useSession } from '../../stores/session'

const props = defineProps<{ tenant: Tenant }>()
const scope = useScope()
const session = useSession()

// The provider decides which names lead to the controllers.
const canManage = computed(() => session.me?.provider_admin ?? false)
const domain = ref('')
const error = ref('')
const busy = ref(false)

async function bind() {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/tenants/{id}/domains', {
        params: { path: { id: props.tenant.id } },
        body: { domain: domain.value },
      }),
    )
    domain.value = ''
    await scope.load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function unbind(name: string) {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.DELETE('/api/v1/tenants/{id}/domains/{domain}', { params: { path: { id: props.tenant.id, domain: name } } }),
    )
    await scope.load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section v-if="canManage || tenant.domains.length" class="domains">
    <h3>Domains</h3>
    <p class="muted note">
      People who sign in at one of these addresses start in {{ tenant.name }}: they leave the tenant empty, or enter a
      sub-tenant below it. The name’s DNS and your HTTPS proxy’s certificate must lead to the controllers.
    </p>
    <ul v-if="tenant.domains.length" class="list">
      <li v-for="d in tenant.domains" :key="d">
        <span class="mono">{{ d }}</span>
        <button v-if="canManage" class="btn small danger" :disabled="busy" @click="unbind(d)">Remove</button>
      </li>
    </ul>
    <form v-if="canManage" class="add" @submit.prevent="bind">
      <label class="field">
        <span class="label">Domain name</span>
        <input id="tenant-domain" v-model="domain" required placeholder="mst.example.com" autocomplete="off" />
      </label>
      <button class="btn" :disabled="busy || !domain.trim()">Bind domain</button>
    </form>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>
  </section>
</template>

<style scoped>
.domains {
  display: grid;
  gap: 10px;
  padding-top: 16px;
  border-top: 1px solid var(--rule);
}
h3 {
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.note {
  max-width: 72ch;
  margin: 0;
  font-size: 13px;
}
/* Bound names, a ruled list. */
.list {
  margin: 0;
  padding: 0;
  list-style: none;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.list li {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  overflow-wrap: anywhere;
}
.list li + li {
  border-top: 1px solid var(--rule);
}
.list .btn {
  margin-left: auto;
}
.add {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: 8px;
}
.add .field {
  flex: 1 1 200px;
  max-width: 360px;
}
.error-text {
  margin: 0;
}
</style>
