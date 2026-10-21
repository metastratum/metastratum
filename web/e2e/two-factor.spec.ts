import { createHmac } from 'node:crypto'
import { expect, request, test, type APIRequestContext, type CDPSession, type Page } from '@playwright/test'

test.skip(!process.env.MST_E2E_URL, 'set MST_E2E_URL to a running controller')

// Browsers use security keys at host names only, and without HTTPS only at
// localhost: so these tests reach the controller there.
const base = (process.env.MST_E2E_URL ?? '').replace('127.0.0.1', 'localhost')
test.use({ baseURL: base })

const admin = { user: process.env.MST_E2E_USER ?? 'admin', password: process.env.MST_E2E_PASSWORD ?? '' }
const PASSWORD = 'e2e-password-1'
const run = `tf${Date.now().toString(36)}`
let api: APIRequestContext
let tenantId = ''

/** RFC 6238, as authenticator apps do it. */
function totp(secret: string, step: number): string {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'
  let bits = ''
  for (const c of secret.replace(/\s/g, '')) bits += alphabet.indexOf(c).toString(2).padStart(5, '0')
  const key = Buffer.from(bits.match(/.{8}/g)!.map((b) => parseInt(b, 2)))
  const counter = Buffer.alloc(8)
  counter.writeBigUInt64BE(BigInt(step))
  const mac = createHmac('sha1', key).update(counter).digest()
  const o = mac[19] & 0x0f
  return String((mac.readUInt32BE(o) & 0x7fffffff) % 1_000_000).padStart(6, '0')
}
const step = () => Math.floor(Date.now() / 30_000)

test.beforeAll(async () => {
  api = await request.newContext({ baseURL: base })
  expect((await api.post('/api/v1/auth/login', { data: { username: admin.user, password: admin.password } })).ok()).toBe(true)
  const root = (await (await api.get('/api/v1/auth/me')).json()).tenant_id
  const t = await api.post('/api/v1/tenants', { data: { parent_id: root, slug: run, name: `Two-factor ${run}` } })
  tenantId = (await t.json()).id
  for (const username of ['pat', 'quinn']) {
    const u = await api.post('/api/v1/users', {
      data: { tenant_id: tenantId, username, display_name: username, password: PASSWORD, role: 'operator' },
    })
    expect(u.ok()).toBe(true)
  }
})

test.afterAll(async () => {
  if (tenantId) expect((await api.delete(`/api/v1/tenants/${tenantId}`)).status()).toBe(204)
  await api.dispose()
})

async function passwordStep(page: Page, username: string, tenant: string, pw: string) {
  await page.goto('/login')
  await page.getByLabel('Username').waitFor()
  const field = page.getByLabel('Tenant')
  if (await field.isVisible()) await field.fill(tenant)
  await page.getByLabel('Username').fill(username)
  await page.getByLabel('Password').fill(pw)
  await page.getByRole('button', { name: 'Sign in' }).click()
}

async function signOut(page: Page) {
  await page.getByRole('button', { name: 'Sign out' }).click()
  await expect(page.getByRole('heading', { name: /^Sign in/ })).toBeVisible()
}

/** A virtual security key that answers by itself: `u2f` (a FIDO U2F key) or
 *  `ctap2` (FIDO2). */
async function virtualKey(cdp: CDPSession, protocol: 'u2f' | 'ctap2') {
  await cdp.send('WebAuthn.enable')
  const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol,
      ...(protocol === 'ctap2' ? { ctap2Version: 'ctap2_1' as const } : {}),
      transport: 'usb',
      hasResidentKey: protocol === 'ctap2',
      hasUserVerification: protocol === 'ctap2',
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  })
  return authenticatorId
}

async function addKey(page: Page, name: string) {
  await page.getByRole('button', { name: 'Security key', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Your password').fill(PASSWORD)
  await dialog.getByLabel('Name').fill(name)
  await dialog.getByRole('button', { name: 'Continue' }).click()
  await expect(dialog).toBeHidden()
  await expect(page.getByText(name, { exact: true })).toBeVisible()
}

test('an authenticator app, backup codes, and U2F and FIDO2 security keys', async ({ page }) => {
  await passwordStep(page, 'pat', run, PASSWORD)
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await page.goto('/account')
  const panel = page.locator('.two-factor')
  await expect(panel.getByText('Off', { exact: true })).toBeVisible()

  // An authenticator app: scan (here: read) the secret, enter its code.
  await panel.getByRole('button', { name: 'Authenticator app' }).click()
  let dialog = page.getByRole('dialog')
  await dialog.getByLabel('Your password').fill(PASSWORD)
  await dialog.getByRole('button', { name: 'Continue' }).click()
  await expect(dialog.getByRole('img', { name: /QR code/ })).toBeVisible()
  const secret = (await dialog.locator('.secret-key').innerText()).replace(/\s/g, '')
  const enrolled = step()
  await dialog.getByLabel('Code', { exact: true }).fill(totp(secret, enrolled))
  await dialog.getByRole('button', { name: 'Add' }).click()
  await expect(page.getByRole('dialog', { name: 'Save your backup codes' })).toBeVisible()
  const codes = await page.locator('.code-list li').allInnerTexts()
  expect(codes).toHaveLength(10)
  await page.getByRole('button', { name: 'I’ve saved them' }).click()
  await expect(panel.getByText('On', { exact: true })).toBeVisible()
  await expect(panel.getByText('10 of 10 backup codes left')).toBeVisible()

  // Signing in takes a code now: the app's next one, then a backup code.
  await signOut(page)
  await passwordStep(page, 'pat', run, PASSWORD)
  await expect(page.getByRole('heading', { name: 'Confirm it’s you' })).toBeVisible()
  await page.getByLabel('Code').fill('000000')
  await page.getByRole('button', { name: 'Verify' }).click()
  await expect(page.getByRole('alert')).toHaveText('That code isn’t right.')
  await page.getByLabel('Code').fill(totp(secret, enrolled + 1))
  await page.getByRole('button', { name: 'Verify' }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await signOut(page)
  await passwordStep(page, 'pat', run, PASSWORD)
  await page.getByLabel('Code').fill(codes[0])
  await page.getByRole('button', { name: 'Verify' }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

  // A FIDO U2F key, then a FIDO2 one.
  const cdp = await page.context().newCDPSession(page)
  const u2f = await virtualKey(cdp, 'u2f')
  await page.goto('/account')
  await addKey(page, 'U2F key')
  await expect(panel.getByText(/works at localhost/)).toBeVisible()
  await signOut(page)
  await passwordStep(page, 'pat', run, PASSWORD)
  // The key is asked for at once, and answers.
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

  await cdp.send('WebAuthn.removeVirtualAuthenticator', { authenticatorId: u2f })
  await virtualKey(cdp, 'ctap2')
  await page.goto('/account')
  await addKey(page, 'FIDO2 key')
  await signOut(page)
  await passwordStep(page, 'pat', run, PASSWORD)
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

  // Removing one asks for the password.
  await page.goto('/account')
  await panel.locator('li', { hasText: 'U2F key' }).getByRole('button', { name: 'Remove' }).click()
  dialog = page.getByRole('dialog')
  await dialog.getByLabel('Your password').fill(PASSWORD)
  await dialog.getByRole('button', { name: 'Remove' }).click()
  await expect(dialog).toBeHidden()
  await expect(panel.getByText('U2F key', { exact: true })).toBeHidden()
  await expect(panel.getByText('9 of 10 backup codes left')).toBeVisible()
})

test('a tenant requiring a second factor, and an administrator resetting one', async ({ browser, page }) => {
  // The administrator turns the requirement on.
  await passwordStep(page, admin.user, '', admin.password)
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await page.goto(`/tenants/${tenantId}`)
  await page.getByRole('tab', { name: 'Sign-in' }).click()
  const require = page.getByRole('switch', { name: 'Require a second factor' })
  await require.click()
  await expect(require).toHaveAttribute('aria-checked', 'true')

  // Quinn has none: setting one up is all there is until then.
  const ctx = await browser.newContext({ baseURL: base })
  const quinn = await ctx.newPage()
  await passwordStep(quinn, 'quinn', run, PASSWORD)
  await expect(quinn.getByText('Your tenant requires a second factor.')).toBeVisible()
  await expect(quinn.getByRole('navigation', { name: 'Main' })).toBeHidden()
  await quinn.getByRole('button', { name: 'Authenticator app' }).click()
  const dialog = quinn.getByRole('dialog')
  await dialog.getByLabel('Your password').fill(PASSWORD)
  await dialog.getByRole('button', { name: 'Continue' }).click()
  const secret = (await dialog.locator('.secret-key').innerText()).replace(/\s/g, '')
  await dialog.getByLabel('Code', { exact: true }).fill(totp(secret, step()))
  await dialog.getByRole('button', { name: 'Add' }).click()
  await quinn.getByRole('button', { name: 'I’ve saved them' }).click()
  await expect(quinn.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

  // The administrator resets Quinn's, as for a lost phone: Quinn is signed
  // out, and back to setting one up.
  await page.getByRole('tab', { name: 'People' }).click()
  await page.locator('tr', { hasText: 'quinn' }).getByRole('button', { name: 'Reset second factor' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Reset' }).click()
  await expect(page.getByRole('dialog')).toBeHidden()
  await quinn.reload()
  await expect(quinn.getByRole('heading', { name: /^Sign in/ })).toBeVisible()
  await passwordStep(quinn, 'quinn', run, PASSWORD)
  await expect(quinn.getByText('Your tenant requires a second factor.')).toBeVisible()
  await ctx.close()

  await page.getByRole('tab', { name: 'Sign-in' }).click()
  await require.click()
  await expect(require).toHaveAttribute('aria-checked', 'false')
})
