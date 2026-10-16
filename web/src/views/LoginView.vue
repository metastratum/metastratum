<script setup lang="ts">
import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ApiError } from '../api/client'
import { useSession } from '../stores/session'

const session = useSession()
const router = useRouter()
const route = useRoute()

const username = ref('')
const password = ref('')
const tenant = ref('')
const error = ref('')
const busy = ref(false)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    await session.login(username.value, password.value, tenant.value)
    const next = typeof route.query.next === 'string' && route.query.next.startsWith('/') ? route.query.next : '/vms'
    await router.replace(next)
  } catch (e) {
    error.value =
      e instanceof ApiError && e.status === 401
        ? 'That username and password don’t match an account in this tenant.'
        : 'The controller isn’t responding. Check that it is running and try again.'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="page">
    <form class="card" @submit.prevent="submit">
      <span class="tape">hypermon</span>
      <h1>Sign in</h1>
      <label class="field">
        <span class="label">Username</span>
        <input v-model="username" autocomplete="username" required autofocus />
      </label>
      <label class="field">
        <span class="label">Password</span>
        <input v-model="password" type="password" autocomplete="current-password" required />
      </label>
      <label class="field">
        <span class="label">Tenant</span>
        <input v-model="tenant" placeholder="acme/dev" autocomplete="organization" />
        <span class="hint">Leave empty if your account belongs to the provider.</span>
      </label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <button class="btn primary submit" :disabled="busy">{{ busy ? 'Signing in…' : 'Sign in' }}</button>
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
.submit {
  justify-content: center;
  height: 38px;
}
</style>
