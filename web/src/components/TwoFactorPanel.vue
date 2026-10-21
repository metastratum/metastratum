<script setup lang="ts">
import { computed, nextTick, onMounted, ref } from 'vue'
import { renderSVG } from 'uqr'
import { api, ApiError, errorText, ok, type Schemas, type SecondFactor } from '../api/client'
import { ago } from '../state'
import { useSession } from '../stores/session'
import { createCredential, keyErrorText, securityKeysWorkHere } from '../webauthn'
import ModalDialog from './ModalDialog.vue'
import StatusLed from './StatusLed.vue'

// The signed-in user's second factors. `required`: shown in place of the
// whole UI, because their tenant requires one and they have none yet.
defineProps<{ required?: boolean }>()
const emit = defineEmits<{ changed: [] }>()

const session = useSession()
const status = ref<Schemas['TwoFactorStatus'] | null>(null)
const loadError = ref('')
const path = () => ({ params: { path: { id: session.me!.user_id } } })

async function load() {
  try {
    status.value = await ok(api.GET('/api/v1/users/{id}/two-factor', path()))
  } catch (e) {
    loadError.value = errorText(e)
  }
}
onMounted(load)

const KINDS: Record<string, string> = {
  totp: 'Authenticator app',
  webauthn: 'Security key',
  yubico_otp: 'YubiKey OTP',
}
const keysHere = securityKeysWorkHere()
const host = location.hostname
const on = computed(() => (status.value?.factors.length ?? 0) > 0)

// One dialog at a time: adding a factor, new backup codes, or removing one.
type Mode = 'totp' | 'key' | 'yubikey' | 'codes' | 'remove'
const mode = ref<Mode | null>(null)
const step = ref<'password' | 'second' | 'codes'>('password')
const password = ref('')
const name = ref('')
const answer = ref('')
const error = ref('')
const busy = ref(false)
const removing = ref<SecondFactor | null>(null)
const totp = ref<Schemas['TotpEnrollment'] | null>(null)
const keyStart = ref<Schemas['WebauthnEnrollment'] | null>(null)
const codes = ref<string[]>([])
const answerInput = ref<HTMLInputElement | null>(null)

const DEFAULT_NAMES: Record<string, string> = { totp: 'Authenticator app', key: 'Security key', yubikey: 'YubiKey' }
function open(m: Mode, factor?: SecondFactor) {
  mode.value = m
  step.value = 'password'
  password.value = ''
  name.value = DEFAULT_NAMES[m] ?? ''
  answer.value = ''
  error.value = ''
  removing.value = factor ?? null
  totp.value = null
  keyStart.value = null
  codes.value = []
}
// Told once the dialog closes, so backup codes stay up until then (the
// setup screen gives way to the whole UI on a change).
let changed = false
function close() {
  mode.value = null
  if (changed) {
    changed = false
    emit('changed')
  }
}

const title = computed(
  () =>
    ({
      totp: 'Add an authenticator app',
      key: 'Add a security key',
      yubikey: 'Add a YubiKey',
      codes: 'New backup codes',
      remove: `Remove ${removing.value?.name ?? ''}`,
    })[mode.value ?? 'totp'],
)

function failed(e: unknown) {
  error.value = e instanceof ApiError || !(e instanceof Error) ? errorText(e) : keyErrorText(e)
}

async function added(result: Schemas['FactorAdded']) {
  await load()
  changed = true
  if (result.backup_codes) {
    codes.value = result.backup_codes
    step.value = 'codes'
  } else {
    close()
  }
}

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const body = { password: password.value }
    if (mode.value === 'totp' && step.value === 'password') {
      totp.value = await ok(api.POST('/api/v1/users/{id}/two-factor/totp/start', { ...path(), body }))
      step.value = 'second'
      await nextTick()
      answerInput.value?.focus()
    } else if (mode.value === 'totp') {
      const body = { token: totp.value!.token, code: answer.value.trim(), name: name.value }
      await added(await ok(api.POST('/api/v1/users/{id}/two-factor/totp', { ...path(), body })))
    } else if (mode.value === 'key') {
      if (!keyStart.value) {
        keyStart.value = await ok(api.POST('/api/v1/users/{id}/two-factor/webauthn/start', { ...path(), body }))
        step.value = 'second'
      }
      await touchKey()
    } else if (mode.value === 'yubikey') {
      const body = { password: password.value, otp: answer.value.trim(), name: name.value }
      await added(await ok(api.POST('/api/v1/users/{id}/two-factor/yubico-otp', { ...path(), body })))
    } else if (mode.value === 'codes') {
      codes.value = (await ok(api.POST('/api/v1/users/{id}/two-factor/backup-codes', { ...path(), body }))).codes
      step.value = 'codes'
      await load()
    } else if (mode.value === 'remove') {
      await ok(
        api.DELETE('/api/v1/users/{id}/two-factor/factors/{factor_id}', {
          params: { path: { id: session.me!.user_id, factor_id: removing.value!.id } },
          body,
        }),
      )
      await load()
      changed = true
      close()
    }
  } catch (e) {
    failed(e)
    if (mode.value === 'yubikey') answer.value = ''
  } finally {
    busy.value = false
  }
}

// Asks the browser for a new credential; again on "Try again", as some
// browsers want a fresh click after a wait.
async function touchKey() {
  error.value = ''
  busy.value = true
  try {
    const credential = await createCredential(keyStart.value!.options as Record<string, unknown>)
    const body = { token: keyStart.value!.token, credential, name: name.value } as never
    await added(await ok(api.POST('/api/v1/users/{id}/two-factor/webauthn', { ...path(), body })))
  } catch (e) {
    failed(e)
  } finally {
    busy.value = false
  }
}

const qr = computed(() => (totp.value ? renderSVG(totp.value.uri, { border: 2 }) : ''))
const groupedSecret = computed(() => totp.value?.secret.match(/.{1,4}/g)?.join(' ') ?? '')

const copied = ref(false)
async function copyCodes() {
  await navigator.clipboard?.writeText(codes.value.join('\n'))
  copied.value = true
  setTimeout(() => (copied.value = false), 1500)
}
function downloadCodes() {
  const text = `Backup codes for ${session.me!.username} at ${location.host}\nEach works once.\n\n${codes.value.join('\n')}\n`
  const a = document.createElement('a')
  a.href = URL.createObjectURL(new Blob([text], { type: 'text/plain' }))
  a.download = `${location.hostname}-backup-codes.txt`
  a.click()
  URL.revokeObjectURL(a.href)
}

function used(f: SecondFactor) {
  return f.last_used_at ? `used ${ago(f.last_used_at)}` : 'not used yet'
}
</script>

<template>
  <article class="card two-factor">
    <header class="head">
      <h2>Two-factor sign-in</h2>
      <StatusLed v-if="status" :tone="on ? 'on' : 'off'" :label="on ? 'On' : 'Off'" class="state" />
    </header>
    <p v-if="required" class="notice" role="status">
      Your tenant requires a second factor. Add one to continue: you’ll need it, as well as your password, each time
      you sign in.
    </p>
    <p v-else-if="status && !on" class="muted">
      Add a second factor, and signing in takes it as well as your password.
      <template v-if="status.required">Your tenant requires one.</template>
    </p>
    <p v-if="loadError" class="error-text">{{ loadError }}</p>

    <ul v-if="status?.factors.length" class="factors">
      <li v-for="f in status.factors" :key="f.id">
        <span v-if="f.name !== KINDS[f.kind]" class="tag">{{ KINDS[f.kind] ?? f.kind }}</span>
        <span class="name">{{ f.name }}</span>
        <span class="muted meta">
          added {{ ago(f.created_at) }} · {{ used(f) }}<template v-if="f.site"> · works at {{ f.site }}</template>
        </span>
        <button type="button" class="btn small danger" @click="open('remove', f)">Remove</button>
      </li>
    </ul>

    <div v-if="on && status" class="codes-row">
      <span class="muted small">{{ status.backup_codes_left }} of 10 backup codes left</span>
      <button type="button" class="btn small" @click="open('codes')">New backup codes</button>
    </div>

    <div v-if="status" class="add">
      <span class="label">Add</span>
      <button type="button" class="btn small" @click="open('totp')">Authenticator app</button>
      <button
        type="button"
        class="btn small"
        :disabled="!keysHere"
        :title="keysHere ? '' : 'Browsers use security keys only at a domain name over HTTPS (or at localhost).'"
        @click="open('key')"
      >
        Security key
      </button>
      <button v-if="status.yubico_otp" type="button" class="btn small" @click="open('yubikey')">YubiKey OTP</button>
    </div>
    <p v-if="status && !keysHere" class="muted small">
      Security keys can’t be added at this address: browsers use them only at a domain name over HTTPS.
    </p>
  </article>

  <ModalDialog v-if="mode" :title="step === 'codes' ? 'Save your backup codes' : title" @close="close">
    <!-- Backup codes, shown once. -->
    <div v-if="step === 'codes'" class="form">
      <p>
        If you lose your second factors, each of these signs you in once. Keep them somewhere safe; they won’t be shown
        again.
      </p>
      <ol class="code-list mono">
        <li v-for="c in codes" :key="c">{{ c }}</li>
      </ol>
      <footer>
        <button type="button" class="btn" @click="copyCodes">{{ copied ? 'Copied' : 'Copy' }}</button>
        <button type="button" class="btn" @click="downloadCodes">Download</button>
        <button type="button" class="btn primary" @click="close">I’ve saved them</button>
      </footer>
    </div>

    <form v-else class="form" @submit.prevent="submit">
      <template v-if="step === 'password'">
        <p v-if="mode === 'remove'">
          Signing in will no longer accept {{ removing?.name }}.
          <template v-if="status?.factors.length === 1">
            It’s your last second factor: your backup codes go with it, and signing in takes only your password.
          </template>
        </p>
        <p v-else-if="mode === 'codes'">The codes you have now stop working.</p>
        <p v-else-if="mode === 'key'">
          Any FIDO2 or U2F security key works, as do passkeys. It will work at <span class="mono">{{ host }}</span>.
        </p>
        <label class="field">
          <span class="label">Your password</span>
          <input v-model="password" type="password" required autocomplete="current-password" autofocus />
        </label>
        <label v-if="mode === 'key' || mode === 'yubikey'" class="field">
          <span class="label">Name</span>
          <input v-model="name" required maxlength="64" />
          <span class="hint">So you can tell your keys apart.</span>
        </label>
        <label v-if="mode === 'yubikey'" class="field">
          <span class="label">YubiKey</span>
          <input v-model="answer" required class="mono" autocomplete="off" spellcheck="false" placeholder="Touch your YubiKey" />
          <span class="hint">With the cursor here, touch the key: it types a one-time password.</span>
        </label>
      </template>

      <template v-else-if="mode === 'totp' && totp">
        <p>Scan this with your authenticator app, then enter the code it shows.</p>
        <div class="qr" role="img" aria-label="QR code for your authenticator app" v-html="qr" />
        <p class="muted small">
          Can’t scan it? Enter this key instead: <span class="mono secret-key">{{ groupedSecret }}</span>
        </p>
        <div class="pair">
          <label class="field">
            <span class="label">Code</span>
            <input
              ref="answerInput"
              v-model="answer"
              required
              inputmode="numeric"
              autocomplete="one-time-code"
              pattern="[0-9 ]{6,7}"
              class="mono"
            />
          </label>
          <label class="field">
            <span class="label">Name</span>
            <input v-model="name" required maxlength="64" />
          </label>
        </div>
      </template>

      <template v-else-if="mode === 'key'">
        <p>{{ busy ? 'Touch your security key now.' : 'The key didn’t register.' }}</p>
      </template>

      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="close">Cancel</button>
        <button v-if="mode === 'key' && step === 'second'" type="button" class="btn primary" :disabled="busy" @click="touchKey">
          Try again
        </button>
        <button v-else class="btn" :class="mode === 'remove' ? 'danger' : 'primary'" :disabled="busy">
          {{
            mode === 'remove'
              ? 'Remove'
              : mode === 'codes'
                ? 'Make new codes'
                : step === 'password' && mode !== 'yubikey'
                  ? 'Continue'
                  : 'Add'
          }}
        </button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.card {
  display: grid;
  gap: 12px;
  align-content: start;
  padding: 16px 18px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
h2 {
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.state {
  flex: none;
}
p {
  margin: 0;
}
.notice {
  padding: 10px 12px;
  border-left: 3px solid var(--tape);
  background: var(--sunken);
}
/* The factors, a ruled list; each one's Remove at its right. */
.factors {
  margin: 0;
  padding: 0;
  list-style: none;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.factors li {
  position: relative;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 8px;
  padding: 10px 92px 10px 12px;
}
.factors li + li {
  border-top: 1px solid var(--rule);
}
.name {
  font-weight: 500;
}
.meta {
  flex-basis: 100%;
  font-size: 12px;
}
.factors .btn {
  position: absolute;
  top: 10px;
  right: 12px;
}
.add,
.codes-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 10px;
}
.add {
  gap: 6px;
}
.add .label {
  margin-right: 4px;
}
.codes-row .btn {
  margin-left: auto;
}
.pair {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 12px;
}
.small {
  font-size: 12px;
}
.codes-row .small {
  font-size: 13px;
}
.qr {
  justify-self: center;
  width: 200px;
  padding: 0;
  background: #fff;
}
.qr :deep(svg) {
  display: block;
  width: 100%;
  height: auto;
}
.secret-key {
  word-break: break-all;
}
.code-list {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 4px 20px;
  margin: 0;
  padding: 12px 12px 12px 34px;
  border-radius: var(--radius);
  background: var(--sunken);
  font-size: 13px;
}
</style>
