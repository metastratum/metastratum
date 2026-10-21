import { createHmac } from 'node:crypto'
import { expect, test as base, type Page } from '@playwright/test'

// The demo, clicked through as a visitor would: every view's dialogs, with
// a fresh seed for each test. Any page error, console error, or API request
// that reaches the network (the demo answers them all in the page) fails.
const test = base.extend<{ problems: string[] }>({
  problems: async ({ page }, use) => {
    const problems: string[] = []
    page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`))
    page.on('pageerror', (e) => problems.push(`page error: ${e.message}`))
    page.on('request', (r) => new URL(r.url()).pathname.startsWith('/api/') && problems.push(`network: ${r.url()}`))
    // A fresh seed for each test (kept across the test's own navigations).
    await page.addInitScript(() => {
      if (!sessionStorage.getItem('demo-test')) {
        localStorage.clear()
        sessionStorage.setItem('demo-test', '1')
      }
    })
    await use(problems)
    // No task failed along the way (the seed's history has none either).
    const failed = await page.evaluate(async () => {
      const tasks = (await (await fetch('/api/v1/tasks?limit=500')).json()) as { kind: string; state: string; error?: string }[]
      return tasks.filter((t) => t.state === 'failed').map((t) => `${t.kind}: ${t.error}`)
    })
    expect([...problems, ...failed]).toEqual([])
  },
})

async function go(page: Page, route: string) {
  await page.goto(`./#/${route}`)
  await page.waitForLoadState('domcontentloaded')
}

/** The VM detail page's status, once it says `text`. */
async function vmState(page: Page, text: string, timeout = 30_000) {
  await expect(page.locator('.vm .head .title').getByText(text, { exact: true })).toBeVisible({ timeout })
}

async function openVm(page: Page, name: string) {
  await go(page, 'vms')
  await page.locator('tbody tr').filter({ has: page.getByText(name, { exact: true }) }).first().click()
  await expect(page.getByRole('heading', { level: 1, name })).toBeVisible()
}

/** Until no task runs, as the API says (asked in the page, which the demo answers). */
async function idle(page: Page, timeout = 30_000) {
  await expect
    .poll(
      () =>
        page.evaluate(async () => {
          const tasks = (await (await fetch('/api/v1/tasks?limit=50')).json()) as { state: string }[]
          return tasks.filter((t) => t.state === 'pending' || t.state === 'running').length
        }),
      { timeout },
    )
    .toBe(0)
}

/** One of the VM page's sections: Overview, Console, Serial or Settings. */
async function vmSection(page: Page, name: string) {
  await page.getByRole('navigation', { name: 'VM sections' }).getByRole('link', { name, exact: true }).click()
}

/** An item of the VM page's More menu (its label, then a hint). */
async function vmMenu(page: Page, item: string) {
  await page.getByRole('button', { name: 'More', exact: true }).click()
  await page.getByRole('menuitem', { name: new RegExp(`^${item}`) }).click()
}

/** Picks the option whose text matches (once the list has it). */
async function choose(select: import('@playwright/test').Locator, text: RegExp) {
  const find = () =>
    select.evaluate(
      (el, src) => [...(el as HTMLSelectElement).options].find((o) => new RegExp(src).test(o.text))?.value ?? '',
      text.source,
    )
  await expect.poll(find, { message: `an option like ${text}` }).not.toBe('')
  await select.selectOption(await find())
}

const modal = (page: Page) => page.locator('dialog[open], [role=dialog]').last()

/** A form control by the label shown above it. */
const field = (page: Page, label: string | RegExp) =>
  modal(page)
    .locator('label.field')
    .filter({ has: page.locator('span.label', { hasText: typeof label === 'string' ? new RegExp(`^${label}$`) : label }) })
    .locator('input, select, textarea')
    .first()

test.describe('VMs', () => {
  test('a Linux VM: create, resize, interfaces, disks, snapshots, backups, delete', async ({ page, problems }) => {
    void problems
    await go(page, 'vms')
    await page.getByRole('button', { name: 'Create VM' }).click()
    await modal(page).getByLabel('Name').fill('pw-linux')
    await modal(page).getByRole('button', { name: 'Create VM' }).click()
    await expect(page.getByRole('heading', { level: 1, name: 'pw-linux' })).toBeVisible()
    await vmState(page, 'Running')

    // Interfaces: add one, then remove it.
    await vmSection(page, 'Overview')
    await page.getByRole('button', { name: 'Add interface' }).click()
    await page.locator('form.nic-form').getByRole('button', { name: 'Add' }).click()
    await expect(page.getByRole('button', { name: 'Remove…' }).first()).toBeVisible()
    const nics = await page.getByRole('button', { name: 'Remove…' }).count()
    await page.getByRole('button', { name: 'Remove…' }).last().click()
    await page.locator('form.nic-form').getByRole('button', { name: 'Remove' }).click()
    await expect(page.getByRole('button', { name: 'Remove…' })).toHaveCount(nics === 2 ? 0 : nics - 1)

    // A new data disk.
    await page.getByRole('button', { name: 'Add disk…' }).click()
    await modal(page).getByLabel('Name').fill('pw-data')
    await modal(page).getByRole('button', { name: 'Add disk' }).click()
    await expect(page.getByText('pw-data').first()).toBeVisible({ timeout: 20_000 })
    await idle(page)

    // A snapshot, taken while it runs.
    await page.getByRole('button', { name: 'Take snapshot' }).click()
    await page.locator('#snapshot-name').fill('pw-snap')
    await page.getByRole('button', { name: 'Take', exact: true }).click()
    await expect(page.locator('.snapshots').getByRole('button', { name: 'Revert…' })).toBeVisible({ timeout: 20_000 })
    await idle(page)

    // A backup.
    await page.getByRole('button', { name: 'Back up now…' }).click()
    await modal(page).getByRole('button', { name: 'Back up' }).click()
    await expect(page.locator('.backups').getByRole('button', { name: 'Restore…' })).toBeVisible({ timeout: 30_000 })
    await idle(page)

    // Off, then resized and reverted.
    await page.getByRole('button', { name: 'Power off' }).click()
    await vmState(page, 'Stopped')
    await vmMenu(page, 'Resize…')
    await modal(page).getByLabel('vCPUs').fill('4')
    await modal(page).getByRole('button', { name: 'Resize' }).click()
    await expect(page.locator('.resources').getByText('4', { exact: true }).first()).toBeVisible()
    await page.locator('.snapshots').getByRole('button', { name: 'Revert…' }).click()
    await modal(page).getByRole('button', { name: 'Revert' }).click()
    await expect(modal(page)).toBeHidden()
    await idle(page)

    // Restored from its backup, as a new VM.
    await page.locator('.backups').getByRole('button', { name: 'Restore…' }).first().click()
    await modal(page).locator('#restore-name').fill('pw-restored')
    await modal(page).getByRole('button', { name: 'Restore as new VM' }).click()
    await expect(page.getByRole('heading', { level: 1, name: 'pw-restored' })).toBeVisible()
    await vmState(page, 'Running')

    // Deleted, with its disks.
    await openVm(page, 'pw-linux')
    await vmMenu(page, 'Delete VM…')
    const deleteDisks = modal(page).locator('#delete-disks')
    if (await deleteDisks.count()) await deleteDisks.check()
    await modal(page).getByRole('button', { name: /^Delete pw-linux/ }).click()
    await go(page, 'vms')
    await expect(page.getByRole('cell', { name: 'pw-linux', exact: true })).toHaveCount(0, { timeout: 20_000 })
  })

  test('a Windows 11 VM, and a clone of a template', async ({ page, problems }) => {
    void problems
    await go(page, 'vms')
    await page.getByRole('button', { name: 'Create VM' }).click()
    await modal(page).getByLabel('Name').fill('pw-desk')
    await modal(page).getByLabel('Start from').selectOption({ label: 'windows-11' })
    await field(page, 'Memory').selectOption({ label: '4 GiB' })
    await field(page, 'Account').fill('pat')
    await field(page, 'Account password').fill('Correct-Horse-9')
    await modal(page).getByRole('button', { name: 'Create VM' }).click()
    await expect(page.getByRole('heading', { level: 1, name: 'pw-desk' })).toBeVisible()
    await vmState(page, 'Running')

    await openVm(page, 'debian-base')
    await page.getByRole('button', { name: 'Clone…', exact: true }).click()
    await modal(page).getByLabel('Name').fill('pw-clone')
    await modal(page).getByRole('button', { name: 'Clone' }).click()
    await openVm(page, 'pw-clone')
    await vmState(page, 'Running')
  })
})

test('disks: new, changed, moved', async ({ page, problems }) => {
  void problems
  await go(page, 'disks')
  await page.getByRole('button', { name: 'New disk' }).click()
  await modal(page).locator('#new-disk-name').fill('pw-vol')
  await modal(page).locator('#new-disk-size').fill('5')
  await choose(modal(page).locator('#new-disk-pool'), /nfs-bulk/)
  await modal(page).getByRole('button', { name: 'Create disk' }).click()
  const row = page.locator('tbody tr').filter({ hasText: 'pw-vol' })
  await expect(row.getByRole('button', { name: 'Change…' })).toBeVisible({ timeout: 20_000 })

  await row.getByRole('button', { name: 'Change…' }).click()
  await modal(page).locator('#disk-rename').fill('pw-vol2')
  await modal(page).locator('#disk-grow').fill('8')
  await modal(page).getByRole('button', { name: 'Save' }).click()
  const renamed = page.locator('tbody tr').filter({ hasText: 'pw-vol2' })
  await expect(renamed.getByText('8 GiB')).toBeVisible()

  await renamed.getByRole('button', { name: 'Move…' }).click()
  await choose(modal(page).locator('#move-pool'), /ceph-ssd/)
  await modal(page).getByRole('button', { name: 'Move disk' }).click()
  await expect(renamed.getByText(/ceph-ssd/)).toBeVisible({ timeout: 30_000 })
  await expect(renamed.getByRole('button', { name: 'Move…' })).toBeVisible({ timeout: 30_000 })
})

test('images: from a URL, and uploaded', async ({ page, problems }) => {
  void problems
  await go(page, 'images')
  await page.getByRole('button', { name: 'Add image' }).first().click()
  await field(page, 'Name').fill('pw-alpine')
  await field(page, 'Download URL').fill('https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/cloud/alpine.qcow2')
  await modal(page).getByRole('button', { name: 'Add image' }).click()
  const fromUrl = page.locator('tbody tr').filter({ has: page.getByText('pw-alpine', { exact: true }) })
  await expect(fromUrl.getByText('Downloaded', { exact: true })).toBeVisible({ timeout: 60_000 })

  await page.getByRole('button', { name: 'Add image' }).first().click()
  await modal(page).getByLabel('A file on this computer').check()
  const bytes = Buffer.alloc(3 * 1024 * 1024)
  bytes.write('QFI\xfb', 0, 'latin1')
  bytes.writeUInt32BE(3, 4)
  await modal(page).locator('input[type=file]').setInputFiles({ name: 'pw-upload.qcow2', mimeType: 'application/octet-stream', buffer: bytes })
  await expect(field(page, 'Name')).toHaveValue('pw-upload')
  await modal(page).getByRole('button', { name: 'Add and upload' }).click()
  const uploaded = page.locator('tbody tr').filter({ has: page.getByText('pw-upload', { exact: true }) })
  await expect(uploaded.getByText('Uploaded', { exact: true })).toBeVisible({ timeout: 60_000 })
})

/** Works in a tenant, as picked in the sidebar. */
async function inTenant(page: Page, name: RegExp) {
  await choose(page.getByRole('combobox', { name: 'Tenant scope' }), name)
}

test('networks: a VPC and network, a security group, a floating IP, a load balancer, a guardrail', async ({ page, problems }) => {
  void problems
  await go(page, 'networks')
  await inTenant(page, /Northwind Traders/)
  await page.getByRole('button', { name: 'New VPC' }).first().click()
  await field(page, 'Name').fill('pw-vpc')
  await choose(field(page, 'External network'), /internet/)
  await modal(page).getByRole('button', { name: 'Create' }).click()
  const vpc = page.locator('section.vpc').filter({ has: page.getByRole('heading', { name: 'pw-vpc', exact: true }) })
  await expect(vpc.getByText(/via 203\.0\.113\./)).toBeVisible()
  await vpc.getByRole('button', { name: 'Add network' }).click()
  await field(page, 'Name').fill('pw-net')
  await field(page, 'Address range').fill('10.60.0.0/24')
  await modal(page).getByRole('button', { name: 'Create' }).click()
  await expect(vpc.getByText('pw-net', { exact: true })).toBeVisible()

  await go(page, 'security-groups')
  await page.getByRole('button', { name: 'New group' }).click()
  await field(page, 'Name').fill('pw-sg')
  await modal(page).getByRole('button', { name: 'Create group' }).click()
  const group = page.locator('section.group').filter({ has: page.getByRole('heading', { name: 'pw-sg', exact: true }) })
  await group.getByRole('button', { name: 'Add rule' }).click()
  await page.getByRole('group', { name: 'Common rules' }).getByRole('button', { name: 'HTTPS' }).click()
  await page.locator('form').getByRole('button', { name: 'Add rule' }).click()
  await expect(group.getByText(/443/)).toBeVisible()

  await go(page, 'floating-ips')
  await page.getByRole('button', { name: 'Allocate address' }).click()
  await choose(field(page, 'Forward to'), /web-2/)
  await modal(page).getByRole('button', { name: 'Allocate' }).click()
  await expect(page.locator('tbody tr').filter({ hasText: 'web-2' })).toBeVisible()

  await go(page, 'load-balancers')
  await page.getByRole('button', { name: 'Create load balancer' }).click()
  await field(page, 'Name').fill('pw-lb')
  await choose(field(page, 'VPC'), /^webshop$/)
  await modal(page).getByRole('checkbox', { name: /^web-1 / }).check()
  await modal(page).getByRole('button', { name: 'Create' }).click()
  await expect(page.getByText('pw-lb', { exact: true })).toBeVisible()

  await go(page, 'guardrails')
  await page.getByRole('button', { name: 'Add guardrail' }).click()
  await page.getByRole('group', { name: 'Common rules' }).getByRole('button', { name: 'SSH' }).click()
  await page.getByRole('button', { name: 'Block this traffic' }).click()
  await expect(page.locator('.rails').first().getByText(/22/)).toBeVisible()
})

/** A labelled control on the page (outside dialogs). */
const control = (page: Page, label: string) =>
  page
    .locator('main label.field')
    .filter({ has: page.locator('span.label', { hasText: new RegExp(`^${label}$`) }) })
    .locator('input, select')
    .first()

test('tenants: a sub-tenant with quotas, resources, a user and access', async ({ page, problems }) => {
  void problems
  await go(page, 'tenants')
  await page.getByRole('navigation', { name: 'Tenants' }).getByRole('button', { name: 'Stratus Hosting' }).click()
  await page.getByRole('button', { name: 'Add sub-tenant' }).click()
  await field(page, 'Name').fill('Initech')
  await field(page, 'Slug').fill('initech')
  await modal(page).getByRole('button', { name: 'Create tenant' }).click()
  await page.getByRole('navigation', { name: 'Tenants' }).getByRole('button', { name: 'Initech' }).click()
  await expect(page.getByRole('heading', { level: 2, name: 'Initech' })).toBeVisible()

  await page.getByRole('tab', { name: 'Quotas' }).click()
  await page.getByRole('button', { name: 'Change limits' }).click()
  await control(page, 'vCPUs').fill('16')
  await control(page, 'Memory \\(GiB\\)').fill('64')
  await page.getByRole('button', { name: 'Save limits' }).click()
  await expect(page.getByRole('button', { name: 'Change limits' })).toBeVisible()
  await expect(page.locator('main').getByText(/\b16\b/).first()).toBeVisible()

  await page.getByRole('tab', { name: 'Resources' }).click()
  await page.locator('form.grants').getByLabel('ams-prod').check()
  await page.locator('form.grants').getByLabel('ceph-ssd').check()
  await page.locator('form.grants').getByRole('button', { name: 'Save' }).click()
  await expect(page.locator('form.grants').getByRole('status')).toHaveText('Saved.')

  await page.getByRole('tab', { name: 'People' }).click()
  await page.getByRole('button', { name: 'Add user' }).click()
  await field(page, 'Username').fill('peter')
  await field(page, 'Full name').fill('Peter Gibbons')
  await field(page, 'Password').fill('tps-reports-1')
  await modal(page).getByRole('button', { name: 'Add user' }).click()
  await expect(page.locator('main').getByText('Peter Gibbons').first()).toBeVisible()

  await page.getByRole('button', { name: 'Give access' }).click()
  await choose(field(page, 'Person'), /\(oscar,/)
  await modal(page).getByLabel(/^operator/).check()
  await modal(page).getByRole('button', { name: 'Give access' }).click()
  await expect(page.getByRole('row', { name: /^oscar operator/ })).toBeVisible()
})

/** RFC 6238, as authenticator apps do it. */
function totp(base32: string): string {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'
  let bits = ''
  for (const c of base32.replace(/[\s=]/g, '').toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, '0')
  const key = Buffer.from(bits.match(/.{8}/g)!.map((b) => parseInt(b, 2)))
  const counter = Buffer.alloc(8)
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30_000)))
  const h = createHmac('sha1', key).update(counter).digest()
  const o = h[h.length - 1]! & 15
  return String((h.readUInt32BE(o) & 0x7fffffff) % 1_000_000).padStart(6, '0')
}

test('account: an API token, and an authenticator app', async ({ page, problems }) => {
  void problems
  await go(page, 'account')
  await page.getByRole('button', { name: 'Create token' }).click()
  await field(page, 'Name').fill('pw-terraform')
  await modal(page).getByRole('button', { name: 'Create token' }).click()
  await expect(modal(page).getByText(/won’t be shown again/)).toBeVisible()
  await modal(page).getByRole('button', { name: 'Done' }).click()
  await expect(page.getByRole('cell', { name: 'pw-terraform' })).toBeVisible()

  await page.getByRole('button', { name: 'Authenticator app' }).click()
  await field(page, 'Your password').fill('demo')
  await modal(page).getByRole('button', { name: 'Continue' }).click()
  const secret = await modal(page).locator('.secret-key').innerText()
  await field(page, 'Code').fill(totp(secret))
  await field(page, 'Name').fill('pw-phone')
  await modal(page).getByRole('button', { name: 'Add' }).click()
  await expect(modal(page).getByRole('heading', { name: 'Save your backup codes' })).toBeVisible()
  await modal(page).getByRole('button', { name: 'I’ve saved them' }).click()
  await expect(page.getByText('pw-phone')).toBeVisible()
})

test('infrastructure: maintenance, evacuation, a cluster, storage, a repository, an external network', async ({ page, problems }) => {
  void problems
  const plate = (name: string) => page.locator('tbody tr').filter({ has: page.locator('strong', { hasText: new RegExp(`^${name}$`) }) })

  await go(page, 'hosts')
  await plate('rtm-hv02').getByRole('button', { name: 'Manage' }).click()
  await modal(page).getByLabel(/Maintenance/).check()
  await modal(page).getByRole('button', { name: 'Save', exact: true }).click()
  await expect(plate('rtm-hv02').getByText('Maintenance', { exact: true })).toBeVisible()
  await plate('rtm-hv02').getByRole('button', { name: 'Manage' }).click()
  await modal(page).getByLabel(/Maintenance/).uncheck()
  await modal(page).getByRole('button', { name: 'Save', exact: true }).click()
  await expect(plate('rtm-hv02').getByText('Maintenance', { exact: true })).toHaveCount(0)

  await plate('ams-hv02').getByRole('button', { name: 'Manage' }).click()
  await modal(page).getByRole('button', { name: 'Evacuate ams-hv02' }).click()
  await expect(plate('ams-hv02').getByText('Maintenance', { exact: true })).toBeVisible()
  await idle(page, 60_000)
  await expect(plate('ams-hv02').locator('text=web-2')).toHaveCount(0)

  await go(page, 'hosts')
  await page.getByRole('button', { name: 'Add cluster' }).first().click()
  await field(page, 'Name').fill('pw-cluster')
  await modal(page).getByRole('button', { name: 'Save' }).click()
  await expect(page.getByRole('heading', { level: 2, name: 'pw-cluster' })).toBeVisible()

  await go(page, 'storage')
  await page.locator('tbody tr').filter({ hasText: 'nfs-bulk' }).getByRole('button', { name: 'Edit' }).click()
  await field(page, 'Mount options').fill('vers=4.2,hard,noatime')
  await modal(page).getByRole('button', { name: 'Save' }).click()
  await expect(modal(page)).toBeHidden()

  await go(page, 'backup-repositories')
  await page.getByRole('button', { name: 'Add repository' }).first().click()
  await modal(page).locator('#repo-name').fill('pw-repo')
  await modal(page).locator('#repo-server').fill('10.20.0.60')
  await modal(page).locator('#repo-export').fill('/export/pw')
  await modal(page).getByRole('button', { name: 'Add repository' }).click()
  await expect(page.getByText('pw-repo')).toBeVisible()

  await go(page, 'external-networks')
  await page.getByRole('button', { name: 'Add external network' }).first().click()
  await field(page, 'Name').fill('pw-ext')
  await field(page, 'Physical network').fill('external')
  await field(page, 'Range').fill('198.51.100.0/24')
  await field(page, 'Hand out from').fill('198.51.100.10')
  await field(page, 'To').fill('198.51.100.200')
  await modal(page).getByRole('button', { name: 'Save' }).click()
  await expect(page.getByText('pw-ext')).toBeVisible()
})

test('audit: what was done is there', async ({ page, problems }) => {
  void problems
  await go(page, 'vms')
  await page.getByRole('button', { name: 'Create VM' }).click()
  await modal(page).getByLabel('Name').fill('pw-audited')
  await modal(page).getByRole('button', { name: 'Create VM' }).click()
  await expect(page.getByRole('heading', { level: 1, name: 'pw-audited' })).toBeVisible()
  await go(page, 'audit')
  await page.getByRole('button', { name: 'Details of vm.create' }).first().click()
  await expect(page.getByText(/"name": "pw-audited"/)).toBeVisible()
})
