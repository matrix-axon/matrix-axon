import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, signIn } from './helpers'

const ROOT = '!space-one:hs'
const SUBSPACE = '!space-two:hs'
const DISCOVERED = '!long:hs'
const GRANDCHILD = '!space-grandchild:hs'

test('expands a joined subspace and filters its deduplicated rooms', async ({
  page,
}) => {
  const spaceReads: string[] = []
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname
    if (path.includes('/space/') || path.includes('/spaces/')) {
      spaceReads.push(path)
    }
  })
  await page.route('**/v1/rooms', async (route) => {
    const response = await route.fetch()
    const rooms = await response.json()
    await route.fulfill({
      json: {
        data: [
          ...rooms.data,
          {
            account_id: ACCOUNT_ID,
            account_user_id: '@me:hs',
            room_id: GRANDCHILD,
            room_type: 'm.space',
            name: 'Grandchild Space',
            last_activity_ts: 0,
          },
        ],
      },
    })
  })
  await page.route('**/spaces/graph?*', (route) =>
    route.fulfill({
      json: {
        data: {
          links: [
            [ROOT, SUBSPACE],
            [SUBSPACE, ROOT],
            [SUBSPACE, ROOM_ID],
            [SUBSPACE, GRANDCHILD],
          ].map(([parent_room_id, child_room_id]) => ({
            parent_room_id,
            child_room_id,
            source: 'child',
            canonical: false,
            origin_ts: 0,
          })),
        },
      },
    }),
  )
  await page.route('**/spaces/discovered?*', (route) =>
    route.fulfill({
      json: {
        data: {
          memberships: [
            {
              space_id: SUBSPACE,
              room_id: ROOM_ID,
              expires_at: Date.now() + 600_000,
            },
            {
              space_id: SUBSPACE,
              room_id: DISCOVERED,
              expires_at: Date.now() + 600_000,
            },
          ],
        },
      },
    }),
  )
  await signIn(page)
  await page.goto('/')

  const picker = page.getByRole('region', { name: 'Spaces' })
  const root = picker.getByRole('button', {
    name: 'E2E Space One',
    exact: true,
  })
  const child = picker.getByRole('button', {
    name: 'E2E Space Two',
    exact: true,
  })
  const expand = picker.getByRole('button', {
    name: 'Expand E2E Space One subspaces',
  })
  await expect(expand).toHaveAttribute('aria-expanded', 'false')
  await expect(child).toHaveCount(0)
  await root.click()
  const direct = page.getByRole('link', { name: /^E2E Room/ })
  const discovered = page.getByRole('link', { name: /A Room With A Name/ })
  await expect(direct).toHaveCount(1)
  await expect(discovered).toBeVisible()

  await expand.click()
  await expect(child).toBeVisible()
  const collapse = picker.getByRole('button', {
    name: 'Collapse E2E Space One subspaces',
  })
  await expect(collapse).toHaveAttribute('aria-expanded', 'true')
  const rootBox = (await root.boundingBox())!
  const childBox = (await child.boundingBox())!
  const pickerBox = (await picker.boundingBox())!
  expect(childBox.x).toBeGreaterThan(rootBox.x)
  expect(childBox.x + childBox.width).toBeLessThanOrEqual(
    pickerBox.x + pickerBox.width,
  )
  if (page.viewportSize()!.width >= 768) {
    expect(childBox.y).toBeGreaterThan(rootBox.y + rootBox.height)
  } else {
    expect(childBox.x).toBeGreaterThanOrEqual(rootBox.x + rootBox.width)
    expect(childBox.y + childBox.height).toBeLessThanOrEqual(
      pickerBox.y + pickerBox.height,
    )
  }

  await picker
    .getByRole('button', { name: 'Expand E2E Space Two subspaces' })
    .click()
  const grandchild = picker.getByRole('button', {
    name: 'Grandchild Space',
    exact: true,
  })
  await expect(grandchild).toBeVisible()
  const connectors = picker.locator('.space-picker-connector')
  await expect(connectors).toHaveCount(2)
  const colors = await connectors.evaluateAll((lines) =>
    lines.map((line) => getComputedStyle(line).backgroundColor),
  )
  expect(colors[0]).not.toBe('rgba(0, 0, 0, 0)')
  expect(colors[1]).not.toBe('rgba(0, 0, 0, 0)')
  expect(colors[1]).not.toBe(colors[0])
  const line = (await connectors.first().boundingBox())!
  const grandchildBox = (await grandchild.boundingBox())!
  if (page.viewportSize()!.width >= 768) {
    expect(line.x).toBeLessThan(childBox.x)
    expect(line.y).toBeLessThanOrEqual(childBox.y)
    expect(line.y + line.height).toBeGreaterThanOrEqual(
      grandchildBox.y + grandchildBox.height,
    )
  } else {
    expect(line.y).toBeLessThan(childBox.y)
    expect(line.x).toBeLessThanOrEqual(childBox.x)
    expect(line.x + line.width).toBeGreaterThanOrEqual(
      grandchildBox.x + grandchildBox.width,
    )
  }

  await child.click()
  await expect(child).toHaveAttribute('aria-pressed', 'true')
  await expect(direct).toHaveCount(1)
  await page
    .getByRole('searchbox', { name: 'Filter by name' })
    .fill('Far Too Long')
  await expect(direct).toHaveCount(0)
  await expect(discovered).toBeVisible()
  await collapse.click()
  await expect(child).toHaveCount(0)
  await expect(connectors).toHaveCount(0)
  await expect(discovered).toBeVisible()
  expect(spaceReads.sort()).toEqual([
    `/v1/accounts/${ACCOUNT_ID}/spaces/discovered`,
    `/v1/accounts/${ACCOUNT_ID}/spaces/graph`,
  ])
})

test('space names are optional and remembered on desktop and tablet widths', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1280, height: 900 })
  await page.goto('/')
  const picker = page.getByRole('region', { name: 'Spaces' })
  const toggle = picker.getByRole('button', { name: 'Show space names' })
  const label = picker
    .locator('.space-picker-name')
    .filter({ hasText: 'E2E Space One' })
  await expect(label).toHaveCount(1)
  await expect(label).toBeHidden()
  const compactWidth = (await picker.boundingBox())!.width
  await toggle.click()
  await expect(toggle).toHaveAttribute('aria-pressed', 'true')
  await expect(label).toBeVisible()
  expect((await picker.boundingBox())!.width).toBeGreaterThan(compactWidth)
  await page.reload()
  await expect(label).toBeVisible()

  // iPad mini in portrait still uses the horizontal picker.
  await page.setViewportSize({ width: 744, height: 1133 })
  await expect(toggle).toBeVisible()
  await expect(label).toBeVisible()
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(toggle).toBeHidden()
  await expect(label).toBeHidden()

  await page.setViewportSize({ width: 820, height: 1180 })
  await expect(label).toBeVisible()
  await toggle.click()
  await expect(toggle).toHaveAttribute('aria-pressed', 'false')
  await expect(label).toBeHidden()
})
