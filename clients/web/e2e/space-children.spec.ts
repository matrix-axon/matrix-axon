import { expect, test, type Page } from '@playwright/test'
import { ACCOUNT_ID, signIn } from './helpers'

/**
 * Room Information for a space (ADR 0133 decisions 6–8): children page from
 * `space/hierarchy` with a count and "Load more", unjoined children are named
 * from the space directory with their provenance, and opening one previews it
 * from `/discovery` before joining. The mock's Space One has 120 children:
 * the joined seeded room, 118 the directory named, and one not looked up.
 */
const SPACE_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent('!space-one:hs')}`

interface HierarchyRead {
  limit: string | null
  cursor: string | null
  enrich: string | null
}

/** Record every hierarchy read the page makes. */
function recordHierarchyReads(page: Page): HierarchyRead[] {
  const reads: HierarchyRead[] = []
  page.on('request', (request) => {
    const url = new URL(request.url())
    if (!url.pathname.endsWith('/space/hierarchy')) return
    reads.push({
      limit: url.searchParams.get('limit'),
      cursor: url.searchParams.get('cursor'),
      enrich: url.searchParams.get('enrich'),
    })
  })
  return reads
}

async function openSpaceInfo(page: Page) {
  await page.goto(SPACE_URL)
  await page
    .getByRole('button', { name: 'Open room information' })
    .first()
    .click()
  const panel = page.getByRole('complementary', { name: 'Room information' })
  await expect(panel).toBeVisible()
  return panel
}

test('a space pages its children and names unjoined ones from the directory', async ({
  page,
}) => {
  const reads = recordHierarchyReads(page)
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  const panel = await openSpaceInfo(page)

  const count = panel.locator('.room-state-children-count')
  await expect(count).toHaveText('Showing 50 of 120')
  const joined = panel.locator('.room-state-child').filter({
    has: page.getByRole('button', { name: 'Child: E2E Room' }),
  })
  await expect(joined.locator('.room-state-child-note')).toHaveText('Joined')
  const discovered = panel.locator('.room-state-child').filter({
    has: page.getByRole('button', {
      name: 'Child: Discovered room 1',
      exact: true,
    }),
  })
  await expect(discovered.locator('.room-state-child-note')).toContainText(
    'Not joined · From space directory · updated',
  )

  await panel.getByRole('button', { name: 'Load more' }).click()
  await expect(count).toHaveText('Showing 100 of 120')
  await panel.getByRole('button', { name: 'Load more' }).click()
  await expect(count).toHaveText('Showing 120 of 120')
  await expect(panel.getByRole('button', { name: 'Load more' })).toHaveCount(0)
  const last = panel.locator('.room-state-child').last()
  await expect(last.getByRole('button')).toHaveText(
    'Child: !not-looked-up:e2e.test',
  )
  await expect(last.locator('.room-state-child-note')).toHaveText(
    'Not joined · Retrieving',
  )

  // Only opening the panel asks for enrichment (decision 7).
  expect(reads[0]).toEqual({ limit: '50', cursor: null, enrich: 'true' })
  expect(reads.slice(1).map((read) => read.enrich)).toEqual([null, null])
  expect(reads.slice(1).map((read) => read.cursor)).toEqual(['at-50', 'at-100'])
  await expect(panel.getByRole('alert')).toHaveCount(0)
})

test('opening an unjoined child previews it before joining', async ({
  page,
}) => {
  const discoveryReads: string[] = []
  page.on('request', (request) => {
    const url = new URL(request.url())
    if (url.pathname.endsWith('/discovery')) {
      discoveryReads.push(decodeURIComponent(url.pathname.split('/').at(-2)!))
    }
  })
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  const panel = await openSpaceInfo(page)
  // Rendering 50 children asks `/discovery` for none of them.
  await expect(panel.locator('.room-state-child')).toHaveCount(50)
  expect(discoveryReads).toEqual([])

  await panel
    .getByRole('button', { name: 'Child: Discovered room 2', exact: true })
    .click()
  const dialog = page.getByRole('dialog', {
    name: 'Join Child: Discovered room 2',
  })
  const preview = dialog.getByRole('region', { name: 'Room preview' })
  const detail = (label: string) =>
    preview
      .locator('dt')
      .filter({ hasText: new RegExp(`^${label}$`) })
      .locator('+ dd')
  await expect(detail('Name')).toHaveText('Discovered room 2')
  await expect(detail('Members')).toHaveText('5 joined')
  await expect(detail('Join rule')).toHaveText('public')
  // The homeserver did not say: unknown, never "not encrypted".
  await expect(detail('Encryption')).toHaveText('Unknown')
  await expect(detail('Canonical alias')).toHaveText('#discovered-2:e2e.test')
  await expect(preview).toContainText('From space directory · updated')
  await expect.poll(() => discoveryReads).toEqual(['!discovered-2:e2e.test'])

  await dialog.getByRole('button', { name: 'Cancel' }).click()
  await expect(dialog).toBeHidden()
})

test('narrow: a space’s children and the join preview fit a phone', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  const panel = await openSpaceInfo(page)
  await expect(panel.locator('.room-state-children-count')).toHaveText(
    'Showing 50 of 120',
  )
  const panelOverflow = await panel.evaluate((node) => ({
    scroll: node.scrollWidth,
    client: node.clientWidth,
    right: node.getBoundingClientRect().right,
  }))
  expect(panelOverflow.scroll).toBeLessThanOrEqual(panelOverflow.client)
  expect(panelOverflow.right).toBeLessThanOrEqual(390)

  await panel
    .getByRole('button', { name: 'Child: Discovered room 3', exact: true })
    .click()
  const dialog = page.getByRole('dialog', {
    name: 'Join Child: Discovered room 3',
  })
  await expect(
    dialog.getByRole('region', { name: 'Room preview' }),
  ).toContainText('Discovered room 3')
  const card = dialog.locator('.overlay-panel')
  const box = await card.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)
  expect(box!.x + box!.width).toBeLessThanOrEqual(390)
  const cardOverflow = await card.evaluate((node) => ({
    scroll: node.scrollWidth,
    client: node.clientWidth,
  }))
  expect(cardOverflow.scroll).toBeLessThanOrEqual(cardOverflow.client)
  await expect(
    dialog.getByRole('button', { name: 'Join and open' }),
  ).toBeInViewport()
})
