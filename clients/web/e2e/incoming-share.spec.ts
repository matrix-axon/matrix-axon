import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, signIn } from './helpers'
import type { IncomingShare } from '../src/incoming-share/types'

test.skip(
  process.env.AXON_SHARE_E2E !== '1',
  'Uses the separate recovery fixture build entry',
)
const record: IncomingShare = {
  version: 1,
  id: '00000000-0000-4000-8000-000000000001',
  createdAt: 1000,
  session: 'fixture-session',
  accountId: ACCOUNT_ID,
  roomId: '!room:hs',
  text: 'https://example.org/article',
  files: [],
  operation: { id: 'text-operation', state: 'unknown' },
  importFailures: [],
  revision: 0,
  previews: [
    {
      matched_url: 'https://example.org/article',
      title: 'Retained preview',
      description: 'Preserved through recovery',
      provenance: 'source',
    },
  ],
}
test.beforeEach(async ({ page }) => {
  await signIn(page)
  await page.addInitScript((record) => {
    if (localStorage.getItem('fixture.incoming') === null)
      localStorage.setItem('fixture.incoming', JSON.stringify([record]))
  }, record)
})
test('recovery preserves cards and uncertain outcomes until explicit discard', async ({
  page,
}) => {
  const sends: string[] = []
  page.on('request', (request) => {
    if (/\/send(?:-media)?$/.test(request.url())) sends.push(request.url())
  })
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByText('Retained preview', { exact: true }),
  ).toBeVisible()
  await expect(
    page.getByText('Outcome unknown', { exact: false }),
  ).toBeVisible()
  await expect(
    page.getByRole('button', { name: 'Send reviewed share' }),
  ).toBeDisabled()
  await expect(
    page.getByRole('button', { name: 'Allow explicit resend' }),
  ).toBeDisabled()
  await page.getByRole('button', { name: 'Keep draft', exact: true }).click()
  await page.evaluate(() => location.assign('/e2e/incoming-share.html'))
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByText('Retained preview', { exact: true }),
  ).toBeVisible()
  const send = page.getByRole('button', { name: 'Send reviewed share' })
  await send.scrollIntoViewIfNeeded()
  const box = await send.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)
  expect(box!.x + box!.width).toBeLessThanOrEqual(page.viewportSize()!.width)
  expect(box!.height).toBeGreaterThanOrEqual(44)
  await page.getByRole('button', { name: 'Discard share', exact: true }).click()
  await expect(
    page.getByText('No pending shares.', { exact: true }),
  ).toBeVisible()
  expect(sends).toEqual([])
})

test('typing persists in batches and refresh recovers expired ownership', async ({
  page,
}) => {
  const sends: string[] = []
  page.on('request', (request) => {
    if (/\/send(?:-media)?$/.test(request.url())) sends.push(request.url())
  })
  await page.goto('/e2e/incoming-share.html')
  await page.evaluate(
    (record) =>
      localStorage.setItem('fixture.incoming', JSON.stringify([record])),
    {
      ...record,
      operation: { ...record.operation, state: 'ready' as const },
    },
  )
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  const editor = page.getByRole('textbox', {
    name: 'Message or first attachment caption',
  })
  const suffix = 'x'.repeat(200)
  await editor.press('End')
  await editor.pressSequentially(suffix)
  const expected = record.text + suffix
  await expect
    .poll(() =>
      page.evaluate(
        () => JSON.parse(localStorage.getItem('fixture.incoming')!)[0].text,
      ),
    )
    .toBe(expected)
  const writes = await page.evaluate(
    () => JSON.parse(localStorage.getItem('fixture.incoming')!)[0].revision,
  )
  expect(writes).toBeLessThan(10)
  await page.evaluate(() => {
    const records = JSON.parse(localStorage.getItem('fixture.incoming')!)
    delete records[0].owner
    records[0].leaseUntil = 0
    records[0].revision++
    localStorage.setItem('fixture.incoming', JSON.stringify(records))
    window.dispatchEvent(new Event('focus'))
  })
  await expect(
    page.getByText('Ownership renewed', { exact: false }),
  ).toBeVisible()
  await expect(editor).toHaveValue(expected)
  await editor.fill('Reclaimed draft https://example.org/article')
  await page.getByRole('button', { name: 'Keep draft', exact: true }).click()
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(editor).toHaveValue(
    'Reclaimed draft https://example.org/article',
  )
  await page
    .getByRole('combobox', { name: 'Room', exact: true })
    .selectOption('')
  await expect(
    page.getByRole('button', { name: 'Send reviewed share' }),
  ).toBeDisabled()
  expect(sends).toEqual([])
})

test('HTTP native sharing explains the restriction and retains the draft', async ({
  page,
}) => {
  const sends: string[] = []
  page.on('request', (request) => {
    if (/\/send(?:-media)?$/.test(request.url())) sends.push(request.url())
  })
  await page.goto('/e2e/incoming-share.html')
  await page.evaluate(
    (record) => {
      localStorage.setItem('fixture.incoming', JSON.stringify([record]))
      localStorage.setItem('fixture.httpSharing', 'true')
    },
    { ...record, operation: { ...record.operation, state: 'ready' as const } },
  )
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await page.getByRole('button', { name: 'Send reviewed share' }).click()
  await expect(page.getByRole('alert')).toContainText(
    'requires an HTTPS server',
  )
  await expect(
    page.getByRole('textbox', { name: 'Message or first attachment caption' }),
  ).toHaveValue(record.text)
  expect(sends).toEqual([])
})
