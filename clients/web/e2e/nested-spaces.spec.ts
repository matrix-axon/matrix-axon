import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, signIn } from './helpers'

const ROOT = '!space-one:hs'
const SUBSPACE = '!space-two:hs'
const DISCOVERED = '!long:hs'

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
  await page.route('**/spaces/graph?*', (route) =>
    route.fulfill({
      json: {
        data: {
          links: [
            [ROOT, SUBSPACE],
            [SUBSPACE, ROOT],
            [SUBSPACE, ROOM_ID],
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
  await expect(discovered).toBeVisible()
  expect(spaceReads.sort()).toEqual([
    `/v1/accounts/${ACCOUNT_ID}/spaces/discovered`,
    `/v1/accounts/${ACCOUNT_ID}/spaces/graph`,
  ])
})
