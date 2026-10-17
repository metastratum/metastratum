import { expect, test, type Page } from '@playwright/test'

test.skip(!process.env.MST_E2E_URL, 'set MST_E2E_URL to a running controller')

// The controller's Content-Security-Policy must not block anything the UI
// does (an inline script or a third-party resource would be refused).
const cspViolations: string[] = []
test.beforeEach(({ page }) => {
  cspViolations.length = 0
  page.on('console', (msg) => {
    if (msg.text().includes('Content Security Policy')) cspViolations.push(msg.text())
  })
})
test.afterEach(() => {
  expect(cspViolations).toEqual([])
})

const user = process.env.MST_E2E_USER ?? 'admin'
const password = process.env.MST_E2E_PASSWORD ?? ''

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
    [/^(Tenants|People)$/, /^(Tenants|People)$/],
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

  // Live-migrate it from the VM page; it keeps running on another host.
  await page.getByRole('tab', { name: 'Details' }).click()
  const host = page.locator('.facts div', { hasText: 'Host' }).locator('dd')
  const before = (await host.textContent())!.trim()
  await page.getByRole('button', { name: 'Migrate' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Migrate' }).click()
  await expect(host).not.toHaveText(before, { timeout: 3 * 60_000 })
  await expect(page.getByText('Running', { exact: true })).toBeVisible({ timeout: 60_000 })

  // A snapshot of the running VM; reverting asks for it to be stopped.
  const snapshots = page.locator('section.snapshots')
  await snapshots.getByRole('button', { name: 'Take snapshot' }).click()
  await snapshots.getByLabel('Name').fill('first')
  await snapshots.getByRole('button', { name: 'Take', exact: true }).click()
  const first = snapshots.getByRole('listitem').filter({ hasText: 'first' })
  await expect(first.getByText('Crash-consistent')).toBeVisible({ timeout: 60_000 })
  await first.getByRole('button', { name: 'Revert…' }).click()
  await expect(page.getByRole('dialog')).toContainText('only a stopped VM can be reverted')
  await expect(page.getByRole('dialog').getByRole('button', { name: 'Revert' })).toBeDisabled()
  await page.getByRole('dialog').getByRole('button', { name: 'Cancel' }).click()
  await first.getByRole('button', { name: 'Delete…' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete snapshot' }).click()
  await expect(first).toHaveCount(0, { timeout: 60_000 })

  // Grow its disk while it runs; CPUs and memory wait for it to stop.
  await page.getByRole('button', { name: 'Resize' }).click()
  const resize = page.getByRole('dialog')
  await expect(resize.getByLabel('vCPUs')).toBeDisabled()
  await resize.getByLabel('Boot disk (GiB)').fill('6')
  await resize.getByRole('button', { name: 'Resize' }).click()
  await expect(resize).toHaveCount(0)
  await expect(page.locator('.facts div', { hasText: 'Disk' }).locator('dd')).toHaveText('6 GiB')

  await page.getByRole('button', { name: 'Delete VM' }).click()
  await page.getByRole('button', { name: `Delete ${name} and its disk` }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await expect(page.getByRole('link', { name, exact: true })).toHaveCount(0, { timeout: 2 * 60_000 })
})

test('manage a sub-tenant: quotas, people, removal', async ({ page }) => {
  const id = Date.now().toString(36)
  const name = `E2E ${id}`
  await signIn(page)
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: /^(Tenants|People)$/ }).click()

  // Create it under the provider: from the People page while the provider
  // is alone, else from the tree.
  const tree = page.getByRole('navigation', { name: 'Tenants' })
  const alone = (await (await page.request.get('/api/v1/tenants')).json()).length === 1
  if (alone) {
    await page.getByRole('button', { name: 'Add tenant' }).click()
  } else {
    await tree.getByRole('button').first().click()
    await page.getByRole('button', { name: 'Add sub-tenant' }).click()
  }
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
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: /^(Tenants|People)$/ }).click()
  await expect(page.getByRole('heading', { name: /^(Tenants|People)$/, level: 1 })).toBeVisible()
  // With the provider alone the tree appears once the tenant below exists.
  const tree = page.getByRole('navigation', { name: 'Tenants' })

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

test('destructive operations say what they remove, and ask first', async ({ page }) => {
  await signIn(page)
  const run = Date.now().toString(36)

  // An image (never downloaded): its dialog warns, then deletes it.
  const image = await page.request.post('/api/v1/images', {
    data: { name: `e2e-img-${run}`, source_url: 'https://example.invalid/never.qcow2', format: 'qcow2' },
  })
  expect(image.ok()).toBeTruthy()
  await page.goto('/images')
  const row = page.getByRole('listitem').filter({ hasText: `e2e-img-${run}` })
  await row.getByRole('button', { name: 'Delete…' }).click()
  const imageDialog = page.getByRole('dialog')
  await expect(imageDialog).toContainText('Every host deletes its downloaded copy')
  await imageDialog.getByRole('button', { name: `Delete e2e-img-${run}` }).click()
  await expect(page.getByText(`e2e-img-${run}`)).toHaveCount(0)

  // Pools: Unlink (the main action) leaves the storage alone and says so;
  // Delete (in the menu) says what it removes and needs the name typed.
  const addPool = async (name: string) => {
    const r = await page.request.post('/api/v1/storage-pools', {
      data: { name, kind: 'nfs', cluster_ids: [], config: { server: '10.77.0.10', export: `/srv/nfs/e2e-unused-${run}` } },
    })
    expect(r.ok()).toBeTruthy()
  }
  await addPool(`e2e-keep-${run}`)
  await addPool(`e2e-drop-${run}`)
  await page.goto('/storage')
  await page.getByRole('row').filter({ hasText: `e2e-keep-${run}` }).getByRole('button', { name: 'Unlink…' }).click()
  const unlinkDialog = page.getByRole('dialog')
  await expect(unlinkDialog).toContainText('Its storage is left exactly as it is')
  await unlinkDialog.getByRole('button', { name: 'Unlink pool' }).click()
  await expect(page.getByText(`e2e-keep-${run}`)).toHaveCount(0)

  const dropRow = page.getByRole('row').filter({ hasText: `e2e-drop-${run}` })
  await dropRow.getByRole('button', { name: `More ways to remove e2e-drop-${run}` }).click()
  await page.getByRole('menuitem', { name: 'Delete…' }).click()
  const poolDialog = page.getByRole('dialog')
  await expect(poolDialog).toContainText('are deleted')
  const deletePool = poolDialog.getByRole('button', { name: 'Delete pool' })
  await expect(deletePool).toBeDisabled()
  await poolDialog.getByLabel(/to confirm/).fill('not the name')
  await expect(deletePool).toBeDisabled()
  await poolDialog.getByLabel(/to confirm/).fill(`e2e-drop-${run}`)
  await expect(deletePool).toBeEnabled()
  await deletePool.click()
  await expect(page.getByText(`e2e-drop-${run}`)).toHaveCount(0)

  // Decommissioning a host: everything it removes is spelled out, and it
  // waits for the name. (Cancelled here.)
  await page.goto('/hosts')
  const hosts = await (await page.request.get('/api/v1/hosts')).json()
  const idle = hosts.find((h: { vm_count: number; state: string }) => !h.vm_count && h.state !== 'decommissioned')
  test.skip(!idle, 'no host without VMs to open the dialog for')
  await page
    .locator('article', { hasText: idle.name, hasNotText: 'Decommissioned' })
    .getByRole('button', { name: 'Manage' })
    .click()
  // Unlink: the machine is left alone; Delete: metastratum is removed from it,
  // OVN stays. Both wait for the name. (Cancelled here.)
  for (const [action, says] of [
    ['Unlink', 'Nothing on the machine changes'],
    ['Delete', 'OVN stays as you set it up'],
  ]) {
    if (action === 'Unlink') {
      await page.getByRole('dialog').getByRole('button', { name: 'Unlink…' }).click()
    } else {
      await page
        .locator('article', { hasText: idle.name, hasNotText: 'Decommissioned' })
        .getByRole('button', { name: 'Manage' })
        .click()
      const more = page.getByRole('dialog').getByRole('button', { name: `More ways to decommission ${idle.name}` })
      // Escape closes the menu, not the dialog around it.
      await more.click()
      await expect(page.getByRole('menuitem', { name: 'Delete…' })).toBeVisible()
      await page.keyboard.press('Escape')
      await expect(page.getByRole('menu')).toHaveCount(0)
      await expect(page.getByRole('dialog')).toBeVisible()
      // The keyboard opens it too.
      await more.press('ArrowDown')
      await expect(page.getByRole('menuitem', { name: 'Delete…' })).toBeFocused()
      await page.keyboard.press('Enter')
    }
    const hostDialog = page.getByRole('dialog')
    await expect(hostDialog).toContainText(says)
    await expect(hostDialog).toContainText('can’t be undone')
    const go = hostDialog.getByRole('button', { name: `${action} ${idle.name}` })
    await expect(go).toBeDisabled()
    await hostDialog.getByLabel(/to confirm/).fill(idle.name)
    await expect(go).toBeEnabled()
    await hostDialog.getByRole('button', { name: 'Cancel' }).click()
    await expect(page.getByRole('dialog')).toHaveCount(0)
  }
})

test('the account page uses the full width, on desktop and phone', async ({ page }) => {
  await signIn(page)
  for (const width of [1280, 400]) {
    await page.setViewportSize({ width, height: 800 })
    await page.goto('/account')
    const roles = page.getByRole('heading', { name: 'Your roles' })
    await expect(roles).toBeVisible()
    // The header's styles once leaked onto this page (it shared a class with
    // the header's account link), squeezing it to 160px or hiding it.
    const box = await page.locator('main > section').boundingBox()
    expect(box!.width).toBeGreaterThan(width * 0.8)
  }
})

test('a domain name signs in to its tenant', async ({ page, browser, baseURL }) => {
  const id = Date.now().toString(36)
  const name = `E2E ${id}`
  await signIn(page)
  const api = page.request
  const me = await (await api.get('/api/v1/auth/me')).json()
  const created = await api.post('/api/v1/tenants', { data: { parent_id: me.tenant_id, slug: `e2e-${id}`, name } })
  const tenant = await created.json()
  let sub: { id: string } | null = null
  try {
    await api.post('/api/v1/users', {
      data: {
        tenant_id: tenant.id,
        username: 'dora',
        display_name: 'Dora',
        password: 'dora-password-1',
        role: 'viewer',
      },
    })
    // Bind a name to it on its Sign-in tab. Chrome sends *.localhost here.
    const domain = `e2e-${id}.localhost`
    await page.goto(`/tenants/${tenant.id}`)
    await page.getByRole('tab', { name: 'Sign-in' }).click()
    await page.getByLabel('Domain name').fill(domain)
    await page.getByRole('button', { name: 'Bind domain' }).click()
    await expect(page.getByText(domain, { exact: true })).toBeVisible()
    // The tab now points people there.
    const there = `http://${domain}:${new URL(baseURL!).port}/login`
    await expect(page.getByRole('link', { name: there })).toBeVisible()

    // There, the page signs in to the tenant, and without sub-tenants it
    // doesn't ask for one.
    const visitor = await (await browser.newContext()).newPage()
    await visitor.goto(there)
    await expect(visitor.getByRole('heading', { name: `Sign in to ${name}` })).toBeVisible()
    await expect(visitor.getByLabel(/tenant/i)).toHaveCount(0)
    await visitor.getByLabel('Username').fill('dora')
    await visitor.getByLabel('Password').fill('dora-password-1')
    await visitor.getByRole('button', { name: 'Sign in' }).click()
    await expect(visitor.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()

    // With a sub-tenant, it asks for one below the tenant.
    sub = await (await api.post('/api/v1/tenants', { data: { parent_id: tenant.id, slug: 'dev', name: 'Dev' } })).json()
    const later = await (await browser.newContext()).newPage()
    await later.goto(there)
    await expect(later.getByLabel('Sub-tenant')).toBeVisible()
    await expect(later.getByText(`Leave empty to sign in to ${name} itself.`)).toBeVisible()
  } finally {
    if (sub) await api.delete(`/api/v1/tenants/${sub.id}`)
    await api.delete(`/api/v1/tenants/${tenant.id}`)
  }
})

test('adding a host shows the agent configuration to write', async ({ page }) => {
  await signIn(page)
  await page.goto('/hosts')
  await page.getByRole('button', { name: 'Add host' }).first().click()
  const dialog = page.getByRole('dialog')
  await dialog.getByRole('button', { name: 'Create join token' }).click()
  // The controllers' own agent endpoints, and the token, ready to paste.
  const config = dialog.locator('pre')
  await expect(config).toContainText('controllers = ["https://')
  await expect(config).toContainText('join_token = "mstjoin.')
  await expect(config).not.toContainText('CONTROLLER')
  await dialog.getByRole('button', { name: 'Done' }).click()
})

test('with the provider alone, tenancy stays out of the way', async ({ page }) => {
  await signIn(page)
  const tenants = await (await page.request.get('/api/v1/tenants')).json()
  test.skip(tenants.length > 1, 'other tenants exist')
  // No tenant to pick, and People instead of Tenants.
  await expect(page.getByLabel('Tenant')).toHaveCount(0)
  const nav = page.getByRole('navigation', { name: 'Main' })
  await nav.getByRole('link', { name: 'People' }).click()
  await expect(page.getByRole('heading', { name: 'People', level: 1 })).toBeVisible()
  await expect(page.getByRole('tab', { name: 'Quotas' })).toHaveCount(0)
  await expect(page.getByRole('tab', { name: 'People' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Add tenant' })).toBeVisible()
  // No guardrails among the network pages.
  await nav.getByRole('link', { name: 'Networks' }).click()
  await expect(page.getByRole('link', { name: 'Security groups' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Guardrails' })).toHaveCount(0)
})

test('backup repositories, and the backups page', async ({ page }) => {
  const id = Date.now().toString(36)
  await signIn(page)
  await page.goto('/backup-repositories')
  await page.getByRole('button', { name: 'Add repository' }).first().click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(`e2e-${id}`)
  await dialog.getByLabel('NFS server').fill('10.77.0.10')
  await dialog.getByLabel('Export').fill('/srv/nfs/backups')
  await dialog.getByRole('button', { name: 'Add repository' }).click()
  const row = page.getByRole('row').filter({ hasText: `e2e-${id}` })
  await expect(row).toContainText('10.77.0.10:/srv/nfs/backups')

  // Backups sit beside the machines.
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Machines' }).click()
  await page.getByRole('navigation', { name: 'Machines' }).getByRole('link', { name: 'Backups' }).click()
  await expect(page.getByRole('heading', { name: 'Backups', level: 1 })).toBeVisible()

  await page.goto('/backup-repositories')
  await row.getByRole('button', { name: 'Delete' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete' }).click()
  await expect(row).toHaveCount(0)
})
