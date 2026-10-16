import { expect, test, type Page } from '@playwright/test'

test.skip(!process.env.HM_E2E_URL, 'set HM_E2E_URL to a running controller')

const user = process.env.HM_E2E_USER ?? 'admin'
const password = process.env.HM_E2E_PASSWORD ?? ''

async function signIn(page: Page) {
  await page.goto('/vms')
  await page.getByLabel('Username').fill(user)
  await page.getByLabel('Password').fill(password)
  await page.getByRole('button', { name: 'Sign in' }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
}

test('sign in, create a VM, use both consoles, delete it', async ({ page }) => {
  const name = `e2e-${Date.now().toString(36)}`
  await signIn(page)

  for (const [link, heading] of [
    ['Images', 'Images'],
    ['Tenants', 'Tenants'],
    ['Audit', 'Audit log'],
    ['Infrastructure', 'Hosts'],
    ['Machines', 'Virtual machines'],
  ]) {
    await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: link }).click()
    await expect(page.getByRole('heading', { name: heading, level: 1 })).toBeVisible()
  }

  await page.getByRole('button', { name: 'Create VM' }).first().click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(name)
  await dialog.getByLabel('Memory').selectOption({ label: '1 GiB' })
  await dialog.getByLabel('Disk (GiB)').fill('5')
  await dialog.getByRole('button', { name: 'Create VM' }).click()

  await expect(page.getByRole('heading', { name, level: 1 })).toBeVisible()
  await expect(page.getByText('Running', { exact: true })).toBeVisible({ timeout: 3 * 60_000 })

  await page.getByRole('tab', { name: 'Console' }).click()
  await expect(page.locator('.screen canvas')).toBeVisible({ timeout: 30_000 })
  await expect(page.getByText('Connected', { exact: true })).toBeVisible()

  await page.getByRole('tab', { name: 'Serial' }).click()
  await expect(page.locator('.xterm')).toBeVisible()
  await expect(page.getByText('Connected', { exact: true })).toBeVisible({ timeout: 30_000 })

  await page.getByRole('button', { name: 'Delete VM' }).click()
  await page.getByRole('button', { name: `Delete ${name} and its disk` }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await expect(page.getByRole('link', { name, exact: true })).toHaveCount(0, { timeout: 2 * 60_000 })
})

test('manage a sub-tenant: quotas, people, removal', async ({ page }) => {
  const id = Date.now().toString(36)
  const name = `E2E ${id}`
  await signIn(page)
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tenants' }).click()

  // Create it under the provider.
  const tree = page.getByRole('navigation', { name: 'Tenants' })
  await tree.getByRole('button').first().click()
  await page.getByRole('button', { name: 'Add sub-tenant' }).click()
  let dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(name)
  // The slug is suggested from the name.
  await expect(dialog.getByLabel('Slug')).toHaveValue(`e2e-${id}`)
  await dialog.getByRole('button', { name: 'Create tenant' }).click()
  await expect(page.getByRole('heading', { name, level: 2 })).toBeVisible()

  // Limit its VMs.
  await page.getByRole('button', { name: 'Change limits' }).click()
  await page.getByLabel('VMs').fill('3')
  await page.getByRole('button', { name: 'Save limits' }).click()
  await expect(page.getByRole('meter', { name: 'VMs used' })).toHaveAttribute('aria-valuemax', '3')

  // Add a user, who gets a role there.
  await page.getByRole('tab', { name: 'People' }).click()
  await page.getByRole('button', { name: 'Add user' }).click()
  dialog = page.getByRole('dialog')
  await dialog.getByLabel('Username').fill('carol')
  await dialog.getByLabel('Password').fill('carol-password-1')
  await dialog.getByRole('button', { name: 'Add user' }).click()
  await expect(page.getByRole('cell', { name: 'carol' }).first()).toBeVisible()

  // Remove the tenant again.
  await page.getByRole('button', { name: 'Delete', exact: true }).first().click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete tenant' }).click()
  await expect(tree.getByRole('button', { name })).toHaveCount(0)
})

test('pages update live when things change elsewhere', async ({ page }) => {
  const id = Date.now().toString(36)
  await signIn(page)
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Tenants' }).click()
  const tree = page.getByRole('navigation', { name: 'Tenants' })
  await expect(tree.getByRole('button').first()).toBeVisible()

  // Change things through the API, as another user or script would.
  const me = await (await page.request.get('/api/v1/auth/me')).json()
  const res = await page.request.post('/api/v1/tenants', {
    data: { parent_id: me.tenant_id, slug: `live-${id}`, name: `Live ${id}` },
  })
  expect(res.ok()).toBeTruthy()
  const tenant = await res.json()
  // No polling would pick this up within the timeout.
  await expect(tree.getByRole('button', { name: `Live ${id}` })).toBeVisible({ timeout: 5_000 })

  await page.request.patch(`/api/v1/tenants/${tenant.id}`, { data: { name: `Renamed ${id}` } })
  await expect(tree.getByRole('button', { name: `Renamed ${id}` })).toBeVisible({ timeout: 5_000 })

  await page.request.delete(`/api/v1/tenants/${tenant.id}`)
  await expect(tree.getByRole('button', { name: `Renamed ${id}` })).toHaveCount(0, { timeout: 5_000 })
})

test('networks: a VPC with a network, a security group with a rule', async ({ page }) => {
  const id = Date.now().toString(36)
  await signIn(page)
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Networks' }).click()
  await expect(page.getByRole('heading', { name: 'VPCs', level: 1 })).toBeVisible()

  await page.getByRole('button', { name: 'New VPC' }).first().click()
  let dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(`vpc-${id}`)
  await dialog.getByRole('button', { name: 'Create' }).click()
  const vpc = page.locator('li.vpc', { hasText: `vpc-${id}` })
  await expect(vpc).toBeVisible()
  await expect(vpc.getByText('Internal only')).toBeVisible()

  await vpc.getByRole('button', { name: 'Add network' }).click()
  dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill('app')
  await dialog.getByLabel('Address range').fill('10.42.0.0/24')
  await dialog.getByRole('button', { name: 'Create' }).click()
  await expect(vpc.getByText('10.42.0.0/24')).toBeVisible()

  await page.getByRole('navigation', { name: 'Networking' }).getByRole('link', { name: 'Security groups' }).click()
  await page.getByRole('button', { name: 'New group' }).click()
  dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(`sg-${id}`)
  await dialog.getByRole('button', { name: 'Create group' }).click()
  const group = page.locator('li.group', { hasText: `sg-${id}` })
  await group.getByRole('button', { name: 'Add rule' }).click()
  await group.getByRole('button', { name: 'HTTPS' }).click()
  await expect(group.getByText('TCP 443 from anywhere').first()).toBeVisible()
  await group.locator('form').getByRole('button', { name: 'Add rule' }).click()
  await expect(group.locator('.rule-row', { hasText: 'TCP 443 from anywhere' })).toBeVisible()

  // Clean up.
  await group.getByRole('button', { name: 'Delete' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete group' }).click()
  await expect(group).toHaveCount(0)
  await page.getByRole('navigation', { name: 'Networking' }).getByRole('link', { name: 'VPCs' }).click()
  await vpc.locator('li.net', { hasText: 'app' }).getByRole('button', { name: 'Delete' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete' }).click()
  await vpc.getByRole('button', { name: 'Delete' }).first().click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete' }).click()
  await expect(vpc).toHaveCount(0)
})
