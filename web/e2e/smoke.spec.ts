import { expect, test } from '@playwright/test'

test.skip(!process.env.HM_E2E_URL, 'set HM_E2E_URL to a running controller')

const user = process.env.HM_E2E_USER ?? 'admin'
const password = process.env.HM_E2E_PASSWORD ?? ''

test('sign in, create a VM, use both consoles, delete it', async ({ page }) => {
  const name = `e2e-${Date.now().toString(36)}`

  await page.goto('/vms')
  await page.getByLabel('Username').fill(user)
  await page.getByLabel('Password').fill(password)
  await page.getByRole('button', { name: 'Sign in' }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

  for (const [link, heading] of [['Images', 'Images'], ['Hosts', 'Hosts'], ['Machines', 'Virtual machines']]) {
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
