import { expect, test, type Locator, type Page } from '@playwright/test'

test.skip(!process.env.MST_E2E_URL, 'set MST_E2E_URL to a running controller')

// The controller's Content-Security-Policy must not block anything the UI
// does (an inline script or a third-party resource would be refused).
const cspViolations: string[] = []
// VMs a test makes are named with this; whatever it left (failing before it
// deleted them) is deleted after it.
let vmPrefix: string | undefined
test.beforeEach(({ page }) => {
  cspViolations.length = 0
  vmPrefix = undefined
  page.on('console', (msg) => {
    if (msg.text().includes('Content Security Policy')) cspViolations.push(msg.text())
  })
})
test.afterEach(async ({ page }) => {
  if (vmPrefix) await deleteVms(page, vmPrefix)
  expect(cspViolations).toEqual([])
})

/** Deletes the VMs named with `prefix` through the API, with their disks,
 *  and waits until they are gone. */
async function deleteVms(page: Page, prefix: string) {
  for (let i = 0; i < 40; i++) {
    const res = await page.request.get('/api/v1/vms')
    if (!res.ok()) return
    const left = ((await res.json()) as { id: string; name: string }[]).filter((v) => v.name.startsWith(prefix))
    if (left.length === 0) {
      const disks = (await (await page.request.get('/api/v1/disks')).json()) as { id: string; name?: string | null }[]
      for (const d of disks.filter((d) => d.name?.startsWith(prefix))) {
        await page.request.delete(`/api/v1/disks/${d.id}`)
      }
      return
    }
    // Busy VMs (still being made, or being deleted) answer 409: again later.
    for (const v of left) await page.request.delete(`/api/v1/vms/${v.id}?delete_disks=true`)
    await page.waitForTimeout(5_000)
  }
  throw new Error(`VMs named ${prefix}… were left behind`)
}

const user = process.env.MST_E2E_USER ?? 'admin'
const password = process.env.MST_E2E_PASSWORD ?? ''

/** A form field by its label's own text (a label around a select also
 *  takes the selected option into its accessible name). */
function field(scope: Locator, label: string) {
  return scope.locator('label.field', { has: scope.page().locator('span.label', { hasText: new RegExp(`^${label}$`) }) }).locator('select, input')
}

async function signIn(page: Page) {
  await page.goto('/vms')
  await page.getByLabel('Username').fill(user)
  await page.getByLabel('Password').fill(password)
  await page.getByRole('button', { name: 'Sign in' }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
}

test('sign in, create a VM, use both consoles, delete it', async ({ page }) => {
  const name = `e2e-${Date.now().toString(36)}`
  vmPrefix = name
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
  await field(dialog, 'Memory').selectOption({ label: '1 GiB' })
  await dialog.getByLabel('CPUs can grow to').selectOption({ label: '4 vCPUs while it runs' })
  await dialog.getByLabel('Memory can grow to').selectOption({ label: '2 GiB while it runs' })
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

  // A disk of its own, plugged in while it runs; detached, it is kept.
  await page.getByRole('button', { name: 'Add disk…' }).click()
  const adding = page.getByRole('dialog')
  await adding.getByLabel('Name').fill(`${name}-data`)
  await adding.getByLabel('Size (GiB)').fill('1')
  await adding.getByRole('button', { name: 'Add disk' }).click()
  const data = page.locator('.disks li', { hasText: `${name}-data` })
  await expect(data).toContainText('/dev/disk/by-id/')
  await data.getByRole('button', { name: 'Detach…' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Detach disk' }).click()
  await expect(data).toHaveCount(0)

  // While it runs: its disk grows, a CPU is plugged in and memory shrinks
  // (through its balloon), within what they can grow to; that waits for it
  // to stop.
  const memory = page.locator('.facts div', { hasText: 'Memory' }).locator('dd')
  const cpus = page.locator('.facts div', { hasText: 'vCPU' }).locator('dd')
  await expect(memory).toContainText('1 GiB / 2 GiB')
  await expect(cpus).toHaveText('2 / 4')
  await page.getByRole('button', { name: 'Resize' }).click()
  const resize = page.getByRole('dialog')
  await expect(resize.getByLabel('CPUs can grow to')).toBeDisabled()
  await expect(resize.getByLabel('Memory can grow to')).toBeDisabled()
  await field(resize, 'vCPUs').fill('3')
  await field(resize, 'Memory').selectOption({ label: '512 MiB' })
  await resize.getByLabel('Boot disk (GiB)').fill('6')
  await resize.getByRole('button', { name: 'Resize' }).click()
  await expect(resize).toHaveCount(0)
  await expect(page.locator('.facts div', { hasText: 'Disk' }).locator('dd')).toHaveText('6 GiB')
  await expect(memory).toContainText('512 MiB / 2 GiB')
  await expect(cpus).toHaveText('3 / 4')
  await expect(memory.locator('meter')).toBeVisible({ timeout: 30_000 })
  await expect(page.getByText('Running', { exact: true })).toBeVisible()

  // Stopped, it becomes a template: never started, cloned instead.
  await page.getByRole('button', { name: 'Power off' }).click()
  await expect(page.getByText('Stopped', { exact: true })).toBeVisible({ timeout: 60_000 })
  // Stopped, it can keep the memory it frees instead of handing it back.
  await expect(page.getByText('Handed back to the host')).toBeVisible()
  await page.getByRole('button', { name: 'Keep it' }).click()
  await expect(page.getByText('Kept by the guest')).toBeVisible()
  await page.getByRole('button', { name: 'Make template' }).click()
  await expect(page.locator('.title .tag', { hasText: 'Template' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Start' })).toBeDisabled()
  await page.getByRole('button', { name: 'Clone' }).click()
  await page.getByRole('dialog').getByLabel('Name').fill(`${name}-c`)
  await page.getByRole('dialog').getByRole('button', { name: 'Clone' }).click()
  await expect(page.getByRole('heading', { name: `${name}-c`, level: 1 })).toBeVisible()
  await expect(page.getByText('Running', { exact: true })).toBeVisible({ timeout: 3 * 60_000 })
  await page.getByRole('button', { name: 'Delete VM' }).click()
  await page.getByRole('button', { name: `Delete ${name}-c and its disk` }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await expect(page.getByRole('link', { name: `${name}-c`, exact: true })).toHaveCount(0, { timeout: 2 * 60_000 })
  await page.getByRole('link', { name, exact: true }).click()
  await expect(page.getByRole('heading', { name, level: 1 })).toBeVisible()

  await page.getByRole('button', { name: 'Delete VM' }).click()
  await page.getByRole('button', { name: `Delete ${name} and its disk` }).click()
  await expect(page.getByRole('heading', { name: 'Virtual machines' })).toBeVisible()
  await expect(page.getByRole('link', { name, exact: true })).toHaveCount(0, { timeout: 2 * 60_000 })

  // The disk it had stays, attached to nothing, until deleted.
  await page.getByRole('link', { name: 'Disks', exact: true }).click()
  const kept = page.getByRole('row').filter({ hasText: `${name}-data` })
  await expect(kept).toContainText('Free')
  await kept.getByRole('button', { name: 'Delete…' }).click()
  await page.getByRole('dialog').getByRole('button', { name: `Delete ${name}-data` }).click()
  await expect(kept).toHaveCount(0)
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

  await group.getByRole('button', { name: 'Delete' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete group' }).click()
  await expect(group).toHaveCount(0)

  // A load balancer with its address inside the VPC.
  await page.getByRole('navigation', { name: 'Networking' }).getByRole('link', { name: 'Load balancers' }).click()
  await expect(page.getByRole('heading', { name: 'Load balancers', level: 1 })).toBeVisible()
  await page.getByRole('button', { name: 'Create load balancer' }).click()
  dialog = page.getByRole('dialog')
  await dialog.getByLabel('Name').fill(`lb-${id}`)
  await dialog.getByRole('combobox', { name: 'VPC', exact: true }).selectOption({ label: `vpc-${id}` })
  await expect(dialog.getByRole('combobox', { name: 'Address', exact: true })).toHaveValue(/.+/)
  await dialog.getByRole('button', { name: 'Create' }).click()
  const lb = page.getByRole('row', { name: new RegExp(`lb-${id}`) })
  await expect(lb).toContainText('TCP 80')
  await expect(lb).toContainText('10.42.0.')
  await lb.getByRole('button', { name: 'Delete…' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete load balancer' }).click()
  await expect(lb).toHaveCount(0)

  // Clean up.
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

  // An image that can't be downloaded: it says so (for good: it is
  // deleted and added again); its dialog warns, then deletes it.
  const image = await page.request.post('/api/v1/images', {
    data: { name: `e2e-img-${run}`, source_url: 'https://example.invalid/never.qcow2', format: 'qcow2' },
  })
  expect(image.ok()).toBeTruthy()
  await page.goto('/images')
  const row = page.getByRole('listitem').filter({ hasText: `e2e-img-${run}` })
  await expect(row.getByText('Download failed', { exact: true })).toBeVisible({ timeout: 60_000 })
  await expect(row).toContainText('Delete it and add it again.')
  await expect(row.getByRole('button', { name: 'Download again' })).toHaveCount(0)
  await row.getByRole('button', { name: 'Delete…' }).click()
  const imageDialog = page.getByRole('dialog')
  await expect(imageDialog).toContainText('Every host deletes its copy')
  await imageDialog.getByRole('button', { name: `Delete e2e-img-${run}` }).click()
  await expect(page.getByText(`e2e-img-${run}`)).toHaveCount(0)

  // Pools: Unlink (the main action) leaves the storage alone and says so;
  // Delete (in the menu) says what it removes and needs the name typed.
  const addPool = async (name: string) => {
    const r = await page.request.post('/api/v1/storage-pools', {
      // An export of its own: two pools on one storage are refused.
      data: { name, kind: 'nfs', cluster_ids: [], config: { server: '10.77.0.10', export: `/srv/nfs/${name}` } },
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

  // A VM backed up from its page: incremental by default, which with
  // nothing to build on yet is taken in full, and says why.
  const images = (await (await page.request.get('/api/v1/images')).json()) as { id: string; name: string }[]
  const image = images.find((i) => i.name === 'e2e-debian-13-shared')
  if (image) {
    vmPrefix = `e2e-bk-${id}`
    const made = await page.request.post('/api/v1/vms', {
      data: { name: vmPrefix, vcpus: 1, memory_mib: 512, disk_gib: 3, image_id: image.id, start: false },
    })
    expect(made.ok()).toBeTruthy()
    await page.goto(`/vms/${((await made.json()) as { id: string }).id}`)
    const backups = page.locator('section.backups')
    await backups.getByRole('button', { name: 'Back up now…' }).click({ timeout: 3 * 60_000 })
    const taking = page.getByRole('dialog')
    if (await taking.getByLabel('Repository').count()) {
      await taking.getByLabel('Repository').selectOption({ label: `e2e-${id}` })
    }
    await expect(taking.getByRole('radio', { name: /^Incremental/ })).toBeChecked()
    await expect(taking).toContainText('so this one is taken in full')
    await taking.getByRole('button', { name: 'Back up', exact: true }).click()
    const taken = backups.getByRole('listitem').first()
    await expect(taken.getByText('Consistent', { exact: true })).toBeVisible({ timeout: 3 * 60_000 })
    await expect(taken.locator('.tag', { hasText: /^Full$/ })).toHaveAttribute('title', /no earlier backup/)
    await taken.getByRole('button', { name: 'Delete…' }).click()
    await page.getByRole('dialog').getByRole('button', { name: 'Delete backup' }).click()
    await expect(backups.getByRole('listitem')).toHaveCount(0, { timeout: 2 * 60_000 })
  }

  // Backups sit beside the machines.
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Machines' }).click()
  await page.getByRole('navigation', { name: 'Machines' }).getByRole('link', { name: 'Backups' }).click()
  await expect(page.getByRole('heading', { name: 'Backups', level: 1 })).toBeVisible()

  await page.goto('/backup-repositories')
  await row.getByRole('button', { name: 'Delete' }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Delete' }).click()
  await expect(row).toHaveCount(0)
})

test('an image uploaded from a file', async ({ page }) => {
  const name = `e2e-up-${Date.now().toString(36)}`
  await signIn(page)
  try {
    await page.goto('/images')
    await page.getByRole('button', { name: 'Add image' }).first().click()
    const dialog = page.getByRole('dialog')
    await dialog.getByRole('radio', { name: 'A file on this computer' }).check()
    // A raw disk of 20 MiB: uploaded in two parts. Its name and format come
    // from the file.
    await dialog.locator('input[type=file]').setInputFiles({
      name: `${name}.raw`,
      mimeType: 'application/octet-stream',
      buffer: Buffer.alloc(20 << 20),
    })
    await expect(field(dialog, 'Name')).toHaveValue(name)
    await expect(field(dialog, 'Format')).toHaveValue('raw')
    await dialog.getByRole('button', { name: 'Add and upload' }).click()
    const row = page.getByRole('listitem').filter({ hasText: name })
    // Checked and stored as a download is: then it shows its disk.
    await expect(row.locator('.facts')).toContainText('disk', { timeout: 3 * 60_000 })
    await expect(row).toContainText('Uploaded,')
    await expect(row.locator('.tag.failed')).toHaveCount(0)
  } finally {
    const images = (await (await page.request.get('/api/v1/images')).json()) as { id: string; name: string }[]
    for (const i of images.filter((i) => i.name === name)) await page.request.delete(`/api/v1/images/${i.id}`)
  }
})
