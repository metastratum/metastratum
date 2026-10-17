<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, ApiError, type Schemas, type SecondFactorChallenge } from '../api/client'
import { useSession } from '../stores/session'
import { getAssertion, keyErrorText, securityKeysWorkHere } from '../webauthn'

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

// The second step, when the account has a second factor.
const challenge = ref<SecondFactorChallenge | null>(null)
const code = ref('')
const codeInput = ref<HTMLInputElement | null>(null)
const methods = computed(() => new Set(challenge.value?.methods ?? []))
const keyOffered = computed(() => !!challenge.value?.webauthn && securityKeysWorkHere())
const codeOffered = computed(() => ['totp', 'yubico_otp', 'backup_code'].some((m) => methods.value.has(m)))
const codeHint = computed(() => {
  const ways = [
    methods.value.has('totp') && 'enter the code from your authenticator app',
    methods.value.has('yubico_otp') && 'touch your YubiKey',
    methods.value.has('backup_code') && 'use a backup code',
  ].filter(Boolean) as string[]
  const text = ways.length > 1 ? `${ways.slice(0, -1).join(', ')} or ${ways[ways.length - 1]}` : (ways[0] ?? '')
  return text.charAt(0).toUpperCase() + text.slice(1) + '.'
})

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const c = await session.login(username.value, password.value, chosen.value)
    if (c) {
      challenge.value = c
      code.value = ''
      busy.value = false
      await nextTick()
      codeInput.value?.focus()
      if (keyOffered.value) await useKey()
      return
    }
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

function secondStepFailed(e: unknown) {
  if (e instanceof ApiError && e.code === 'sign_in_expired') {
    restart()
    error.value = 'That sign-in timed out or had too many wrong answers. Sign in again.'
  } else if (e instanceof ApiError && e.status === 401) {
    error.value = keyOffered.value && !code.value ? 'That key isn’t one of yours.' : 'That code isn’t right.'
  } else if (e instanceof ApiError && (e.status === 429 || e.status === 503)) {
    error.value = e.message
  } else {
    error.value = 'The controller isn’t responding. Check that it is running and try again.'
  }
}

async function verify() {
  // A code instead of the key: stop asking for the key.
  keyAbort?.abort()
  error.value = ''
  busy.value = true
  try {
    await session.secondFactor(challenge.value!.token, { code: code.value.trim() })
    await router.replace(next.value)
  } catch (e) {
    secondStepFailed(e)
    code.value = ''
  } finally {
    busy.value = false
  }
}

// Asking the browser for the key; a code typed meanwhile cancels it.
const waitingForKey = ref(false)
let keyAbort: AbortController | null = null
async function useKey() {
  error.value = ''
  waitingForKey.value = true
  keyAbort = new AbortController()
  const signal = keyAbort.signal
  try {
    const answer = await getAssertion(challenge.value!.webauthn as Record<string, unknown>, signal)
    busy.value = true
    await session.secondFactor(challenge.value!.token, { webauthn: answer })
    await router.replace(next.value)
  } catch (e) {
    if (signal.aborted) return
    if (e instanceof ApiError) secondStepFailed(e)
    else error.value = keyErrorText(e)
  } finally {
    waitingForKey.value = false
    busy.value = false
  }
}

function restart() {
  keyAbort?.abort()
  challenge.value = null
  password.value = ''
  code.value = ''
  error.value = ''
}
</script>

<template>
  <div class="page">
    <form v-if="challenge" class="card" @submit.prevent="verify">
      <span class="tape">metastratum</span>
      <h1>Confirm it’s you</h1>
      <p class="muted lead">{{ username }} signs in with a second factor too.</p>
      <button
        v-if="keyOffered"
        type="button"
        class="btn submit"
        :class="{ primary: !codeOffered }"
        :disabled="busy || waitingForKey"
        @click="useKey"
      >
        {{ waitingForKey ? 'Touch your security key…' : 'Use your security key' }}
      </button>
      <div v-if="keyOffered && codeOffered" class="or label" aria-hidden="true">or with a code</div>
      <label v-if="codeOffered" class="field">
        <span class="label">Code</span>
        <input
          ref="codeInput"
          v-model="code"
          autocomplete="one-time-code"
          autocapitalize="off"
          spellcheck="false"
          class="mono"
        />
        <span class="hint">{{ codeHint }}</span>
      </label>
      <p v-if="!codeOffered && !keyOffered" class="error-text">
        None of your second factors works at this address. Use the address you set them up at, or ask an
        administrator to reset them.
      </p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <button v-if="codeOffered" class="btn primary submit" :disabled="busy || !code.trim()">
        {{ busy ? 'Checking…' : 'Verify' }}
      </button>
      <button type="button" class="link" @click="restart">Start over</button>
    </form>
    <form v-else-if="ready" class="card" @submit.prevent="submit">
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
.error-text,
.lead {
  margin: 0;
}
.link {
  justify-self: center;
  border: 0;
  background: none;
  color: var(--muted);
  font: inherit;
  font-size: 13px;
  text-decoration: underline;
  text-underline-offset: 3px;
  cursor: pointer;
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
