<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, ApiError, type Schemas } from '../api/client'
import { useSession } from '../stores/session'

const session = useSession()
const router = useRouter()
const route = useRoute()

const username = ref('')
const password = ref('')
const tenant = ref(typeof route.query.tenant === 'string' ? route.query.tenant : '')
// Set when an identity provider sent the browser back with a problem.
const error = ref(typeof route.query.error === 'string' ? route.query.error : '')
const busy = ref(false)
const provider = ref<string | null>(null)

// The tenant this address is bound to, and whether there are sub-tenants to
// ask for. Until that is known (or if it can't be), the field is offered.
const site = ref<Schemas['SiteView'] | null>(null)
const ready = ref(false)
const usernameInput = ref<HTMLInputElement | null>(null)
onMounted(async () => {
  const { data } = await api.GET('/api/v1/auth/site').catch(() => ({ data: undefined }))
  site.value = data ?? null
  ready.value = true
  await nextTick()
  usernameInput.value?.focus()
})
const askTenant = computed(() => site.value?.sub_tenants ?? true)
const bound = computed(() => site.value?.tenant_name ?? null)
const chosen = computed(() => (askTenant.value ? tenant.value.trim() : ''))

const next = computed(() =>
  typeof route.query.next === 'string' && route.query.next.startsWith('/') && !route.query.next.startsWith('//')
    ? route.query.next
    : '/vms',
)

// Offer the tenant's identity provider once the tenant is known.
let timer = 0
watch(
  chosen,
  (t) => {
    clearTimeout(timer)
    provider.value = null
    timer = window.setTimeout(async () => {
      const { data } = await api.GET('/api/v1/auth/methods', { params: { query: { tenant: t } } })
      if (t === chosen.value) provider.value = data?.oidc ?? null
    }, 300)
  },
  { immediate: true },
)

const ssoHref = computed(
  () => `/api/v1/auth/oidc/start?${new URLSearchParams({ tenant: chosen.value, return_to: next.value })}`,
)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    await session.login(username.value, password.value, chosen.value)
    await router.replace(next.value)
  } catch (e) {
    error.value =
      e instanceof ApiError && e.status === 401
        ? 'That username and password don’t match an account in this tenant.'
        : e instanceof ApiError && e.status === 429
          ? 'Too many sign-in attempts. Wait a minute, then try again.'
          : 'The controller isn’t responding. Check that it is running and try again.'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="page">
    <form v-if="ready" class="card" @submit.prevent="submit">
      <span class="tape">metastratum</span>
      <h1>{{ bound ? `Sign in to ${bound}` : 'Sign in' }}</h1>
      <label v-if="askTenant" class="field">
        <span class="label">{{ bound ? 'Sub-tenant' : 'Tenant' }}</span>
        <input v-model="tenant" :placeholder="bound ? 'dev' : 'acme/dev'" autocomplete="organization" />
        <span class="hint">{{
          bound ? `Leave empty to sign in to ${bound} itself.` : 'Leave empty if your account belongs to the provider.'
        }}</span>
      </label>
      <a v-if="provider" class="btn primary sso" :href="ssoHref">Continue with {{ provider }}</a>
      <div v-if="provider" class="or label" aria-hidden="true">or with a password</div>
      <label class="field">
        <span class="label">Username</span>
        <input ref="usernameInput" v-model="username" autocomplete="username" :required="!provider" />
      </label>
      <label class="field">
        <span class="label">Password</span>
        <input v-model="password" type="password" autocomplete="current-password" :required="!provider" />
      </label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <button class="btn submit" :class="{ primary: !provider }" :disabled="busy || !username || !password">
        {{ busy ? 'Signing in…' : 'Sign in' }}
      </button>
    </form>
  </div>
</template>

<style scoped>
.page {
  display: grid;
  place-items: center;
  min-height: 100vh;
  padding: 16px;
}
.card {
  display: grid;
  gap: 16px;
  width: min(380px, 100%);
  padding: 28px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  box-shadow: var(--shadow);
}
.tape {
  justify-self: start;
  padding: 3px 10px 2px;
  background: var(--tape);
  color: var(--tape-ink);
  font: 600 13px var(--font-label);
  letter-spacing: 0.14em;
  text-transform: uppercase;
  clip-path: polygon(4px 0, 100% 0, calc(100% - 4px) 100%, 0 100%);
}
h1 {
  font-size: 24px;
}
.error-text {
  margin: 0;
}
.submit,
.sso {
  justify-content: center;
  height: 38px;
  text-decoration: none;
}
.or {
  display: flex;
  align-items: center;
  gap: 10px;
}
.or::before,
.or::after {
  content: '';
  flex: 1;
  border-top: 1px solid var(--rule);
}
</style>
