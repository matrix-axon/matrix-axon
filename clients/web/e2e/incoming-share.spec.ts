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
test('settings retains the recovery route when the inbox is empty', async ({
  page,
}) => {
  await page.addInitScript(() => localStorage.setItem('fixture.incoming', '[]'))
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await page.getByRole('link', { name: 'Pending shares', exact: true }).click()
  await expect(
    page.getByRole('heading', { name: 'Pending shares', exact: true }),
  ).toBeVisible()
  await expect(page.getByText('No pending shares.')).toBeVisible()
})
test('clear all fences idle composers while preserving live sends and new arrivals', async ({
  page,
}, testInfo) => {
  await page.addInitScript((record) => {
    localStorage.setItem(
      'fixture.incoming',
      JSON.stringify([
        { ...record, id: 'first' },
        {
          ...record,
          id: 'active',
          owner: 'extension-owner',
          text: 'Sending in another composer',
          operation: { id: 'sending-op', state: 'dispatching' },
        },
        {
          ...record,
          id: 'third',
          text: 'Idle in another composer',
          owner: 'idle-owner',
          operation: { id: 'third-op', state: 'ready' },
        },
      ]),
    )
  }, record)
  const sends: string[] = []
  page.on('request', (request) => {
    if (/\/send(?:-media)?$/.test(request.url())) sends.push(request.url())
  })
  await page.goto('/e2e/incoming-share.html')
  const entry = page.getByRole('link', {
    name: 'Pending shares, 3',
    exact: true,
  })
  await expect(entry.locator('.topbar-count-badge')).toHaveText('3')
  await expect(entry.locator('.topbar-count-badge')).toBeInViewport()
  await page.getByRole('button', { name: 'Clear all', exact: true }).click()
  await expect(
    page.getByRole('group', { name: 'Clear pending shares' }),
  ).toContainText('Delete 3 saved shares')
  await page.screenshot({
    path: testInfo.outputPath('clear-all-confirmation.png'),
    fullPage: true,
  })
  await page
    .getByRole('button', { name: 'Cancel clear all', exact: true })
    .click()
  await expect(
    page.getByRole('button', { name: 'Review share', exact: true }),
  ).toHaveCount(3)
  await page
    .getByRole('button', { name: 'Review share', exact: true })
    .first()
    .click()
  await page.getByRole('button', { name: 'Clear all', exact: true }).click()
  await page.evaluate((record) => {
    const records = JSON.parse(localStorage.getItem('fixture.incoming')!)
    records.push({
      ...record,
      id: 'arriving',
      text: 'Arrived after confirmation opened',
    })
    localStorage.setItem('fixture.incoming', JSON.stringify(records))
  }, record)
  await page
    .getByRole('button', { name: 'Clear all pending shares', exact: true })
    .click()
  await expect(page.getByRole('main').getByRole('status')).toHaveText(
    'Cleared 2 pending shares.',
  )
  await expect(page.getByRole('alert')).toContainText(
    '1 share could not be cleared',
  )
  await expect(page.getByRole('alert')).toContainText('still sending')
  await expect(
    page
      .getByRole('link', { name: 'Pending shares, 2', exact: true })
      .locator('.topbar-count-badge'),
  ).toHaveText('2')
  await expect(
    page.getByRole('button', { name: 'Review share', exact: true }),
  ).toHaveCount(2)
  await page.evaluate(() => {
    const records = JSON.parse(localStorage.getItem('fixture.incoming')!)
    records.find(
      (item: { id: string }) => item.id === 'active',
    ).operation.state = 'sent'
    localStorage.setItem('fixture.incoming', JSON.stringify(records))
    window.dispatchEvent(new Event('focus'))
  })
  await page.getByRole('button', { name: 'Clear all', exact: true }).click()
  await page
    .getByRole('button', { name: 'Clear all pending shares', exact: true })
    .click()
  await expect(
    page.getByText('No pending shares.', { exact: true }),
  ).toBeVisible()
  await expect(page.getByRole('link', { name: /^Pending shares/ })).toHaveCount(
    0,
  )
  await expect(
    page.getByRole('button', { name: 'Clear all', exact: true }),
  ).toHaveCount(0)
  expect(sends).toEqual([])
})

test('an unreadable inbox keeps a warning entry until retry succeeds', async ({
  page,
}) => {
  await page.addInitScript(() => {
    localStorage.setItem('fixture.incoming', '[]')
    localStorage.setItem('fixture.inboxUnavailable', 'true')
  })
  await page.goto('/e2e/incoming-share.html')
  const entry = page.getByRole('link', {
    name: 'Pending shares unavailable',
    exact: true,
  })
  await expect(entry.locator('.topbar-count-badge')).toHaveText('!')
  await page.evaluate(() => localStorage.removeItem('fixture.inboxUnavailable'))
  await page
    .getByRole('button', { name: 'Refresh pending shares', exact: true })
    .click()
  await expect(page.getByRole('alert')).toHaveCount(0)
  await expect(page.getByRole('link', { name: /^Pending shares/ })).toHaveCount(
    0,
  )
})
test('leaving recovery cancels device acquisition and releases ownership', async ({
  page,
}) => {
  await page.addInitScript((record) => {
    localStorage.setItem(
      'fixture.incoming',
      JSON.stringify([
        {
          ...record,
          previews: [],
          operation: { ...record.operation, state: 'ready' },
        },
      ]),
    )
    localStorage.setItem('fixture.waitForPreview', 'true')
  }, record)
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await page
    .getByRole('button', { name: 'Include preview:', exact: false })
    .click()
  await expect(page.getByRole('textbox', { name: 'Message' })).toBeDisabled()
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await expect
    .poll(() =>
      page.evaluate(
        () => JSON.parse(localStorage.getItem('fixture.incoming')!)[0].owner,
      ),
    )
    .toBeUndefined()
  await page
    .getByRole('link', { name: 'Pending shares, 1', exact: true })
    .click()
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Message' })).toBeEnabled()
})
test('preview work disables the caption and preserves its saved text', async ({
  page,
}) => {
  await page.addInitScript(
    (record) => {
      localStorage.setItem('fixture.incoming', JSON.stringify([record]))
    },
    {
      ...record,
      operation: { ...record.operation, state: 'ready' },
      previews: [],
    },
  )
  let release!: () => void
  const waiting = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/link-previews/generate', async (route) => {
    await waiting
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ data: { status: 'unavailable' } }),
    })
  })
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  const caption = page.getByRole('textbox', {
    name: 'Message',
  })
  await expect(caption).toBeEnabled()
  await page
    .getByRole('button', { name: 'Use homeserver instead:', exact: false })
    .click()
  await expect(caption).toBeEnabled()
  await page
    .getByRole('button', { name: 'Fetch using homeserver', exact: true })
    .click()
  await expect(caption).toBeDisabled()
  await expect(caption).toHaveValue(record.text)
  release()
  await expect(caption).toBeEnabled()
  await expect(caption).toHaveValue(record.text)
})
test('device preview and its failure do not submit a URL to the homeserver', async ({
  page,
}) => {
  await page.addInitScript((record) => {
    localStorage.setItem(
      'fixture.incoming',
      JSON.stringify([
        {
          ...record,
          operation: { ...record.operation, state: 'ready' },
          previews: [],
        },
      ]),
    )
  }, record)
  let lookups = 0
  await page.route('**/link-previews/generate', async (route) => {
    lookups += 1
    await route.fulfill({ json: { data: { status: 'unavailable' } } })
  })
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await page
    .getByRole('button', { name: 'Include preview:', exact: false })
    .click()
  await expect(
    page.getByText('Generated on device', { exact: true }),
  ).toBeVisible()
  expect(lookups).toBe(0)
  await page.evaluate(() =>
    localStorage.setItem('fixture.devicePreviewFailure', 'true'),
  )
  await page
    .getByRole('button', { name: 'Refresh on this device', exact: true })
    .click()
  await expect(page.getByRole('alert')).toContainText(
    'on-device preview lookup timed out',
  )
  expect(lookups).toBe(0)
  await page
    .getByRole('button', { name: 'Use homeserver instead:', exact: false })
    .click()
  expect(lookups).toBe(0)
  await page
    .getByRole('button', { name: 'Fetch using homeserver', exact: true })
    .click()
  await expect.poll(() => lookups).toBe(1)
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
    name: 'Message',
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
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveValue(
    record.text,
  )
  expect(sends).toEqual([])
})

for (const partial of [false, true]) {
  test(`photo recovery ${partial ? 'protects a confirmed caption and uncertain outcomes' : 'previews and removes photos in order'}`, async ({
    page,
  }) => {
    await page.addInitScript(
      ({ record, partial }) => {
        if (localStorage.getItem('fixture.photoSeeded') === 'true') return
        localStorage.setItem('fixture.photoSeeded', 'true')
        localStorage.setItem('fixture.thumbnailFailureOnce', 'true')
        localStorage.setItem(
          'fixture.incoming',
          JSON.stringify([
            {
              ...record,
              version: 4,
              text: 'Independent source message',
              previews: [],
              operation: {
                id: 'batch',
                state: partial ? 'sent' : 'ready',
                ...(partial ? { eventId: '$source' } : {}),
              },
              files: ['first.heic', 'second.png'].map((name, index) => ({
                id: `photo-${index}`,
                name,
                mime: index === 0 ? 'image/heic' : 'image/png',
                bytes: 3 * 1024 * 1024,
                photoSize: 'small',
                photoBytes: 512,
                caption: index === 0 ? 'Photo caption' : 'Second caption',
                digest: 'fixture',
                thumbnail: false,
                operation: {
                  id: `operation-${index}`,
                  state: partial ? (index === 0 ? 'sent' : 'unknown') : 'ready',
                  ...(partial && index === 0 ? { eventId: '$confirmed' } : {}),
                },
              })),
            },
          ]),
        )
      },
      { record, partial },
    )
    await page.goto('/e2e/incoming-share.html')
    await page
      .getByRole('button', { name: 'Review share', exact: true })
      .click()
    const photos = page.getByRole('group', { name: 'Photos in sending order' })
    await photos.getByRole('button', { name: 'Retry image preview' }).click()
    await expect(
      page.getByRole('textbox', { name: 'Shared message', exact: true }),
    ).toHaveValue('Independent source message')
    await expect(photos.getByRole('img', { name: 'Shared photo' })).toHaveCount(
      2,
    )
    await expect(photos.getByRole('img').first()).toBeVisible()
    await expect(photos).toContainText('small · ~1 KiB upload')
    expect(
      await photos
        .getByRole('button', { name: /^Remove photo:/ })
        .allTextContents(),
    ).toEqual(['Remove photo: first.heic', 'Remove photo: second.png'])
    const caption = page.getByRole('textbox', {
      name: 'Caption for first.heic',
    })
    const secondCaption = page.getByRole('textbox', {
      name: 'Caption for second.png',
    })
    await expect(caption).toHaveValue('Photo caption')
    await expect(secondCaption).toHaveValue('Second caption')
    if (partial) {
      await expect(caption).toBeDisabled()
      await expect(secondCaption).toBeDisabled()
      for (const button of await photos.getByRole('button').all())
        await expect(button).toBeDisabled()
      const retry = page.getByRole('button', {
        name: 'Allow explicit resend',
        exact: true,
      })
      await expect(retry).toBeDisabled()
      await page.getByRole('checkbox').check()
      await retry.click()
      await expect(
        page.getByRole('list', { name: 'Delivery outcomes' }),
      ).toContainText('$confirmed')
      await expect(
        page.getByRole('list', { name: 'Delivery outcomes' }),
      ).toContainText('second.png: ready')
    } else {
      const sourceMessage = page.getByRole('textbox', {
        name: 'Shared message',
        exact: true,
      })
      await sourceMessage.fill('')
      await expect(sourceMessage).toBeVisible()
      await sourceMessage.fill('Independent source message')
      await secondCaption.fill('Edited second caption')
      await photos
        .getByRole('button', { name: 'Remove photo: first.heic', exact: true })
        .click()
      await expect(
        photos.getByRole('button', { name: /^Remove photo:/ }),
      ).toHaveCount(1)
      await expect(secondCaption).toHaveValue('Edited second caption')
      await expect(
        page.getByRole('textbox', { name: 'Shared message', exact: true }),
      ).toHaveValue('Independent source message')
      await page
        .getByRole('button', { name: 'Keep draft', exact: true })
        .click()
      await page.goto('/e2e/incoming-share.html')
      await page
        .getByRole('button', { name: 'Review share', exact: true })
        .click()
      await expect(
        page
          .getByRole('group', { name: 'Photos in sending order' })
          .getByRole('button'),
      ).toHaveText('Remove photo: second.png')
      await expect(
        page.getByRole('textbox', { name: 'Caption for second.png' }),
      ).toHaveValue('Edited second caption')
    }
  })
}

test('text-only preview overflow explains the link limit without reporting an unsupported image', async ({
  page,
}) => {
  await page.addInitScript((record) => {
    localStorage.setItem(
      'fixture.incoming',
      JSON.stringify([
        {
          ...record,
          version: 4,
          operation: { ...record.operation, state: 'ready' },
          importFailures: ['too_many_source_previews'],
        },
      ]),
    )
  }, record)
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByText(
      'The source supplied more than eight link previews. Share fewer links.',
      { exact: true },
    ),
  ).toBeVisible()
  await expect(
    page.getByRole('button', { name: 'Send reviewed share' }),
  ).toBeDisabled()
})

test('master caption, drag ordering and close preserve the photo draft on immediate return', async ({
  page,
}) => {
  await page.addInitScript((record) => {
    localStorage.setItem('fixture.strictClaim', 'true')
    localStorage.setItem(
      'fixture.incoming',
      JSON.stringify([
        {
          ...record,
          version: 4,
          text: 'Separate source',
          previews: [],
          operation: { id: 'source', state: 'ready' },
          files: ['first.png', 'second.png'].map((name, index) => ({
            id: `photo-${index}`,
            name,
            mime: 'image/png',
            bytes: 1024,
            digest: 'fixture',
            caption: '',
            thumbnail: false,
            operation: { id: `op-${index}`, state: 'ready' },
          })),
        },
      ]),
    )
  }, record)
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await page
    .getByRole('textbox', { name: 'Master caption', exact: true })
    .fill('Together 🌊')
  await expect(
    page.getByRole('textbox', { name: 'Caption for first.png' }),
  ).toHaveValue('Together 🌊')
  await expect(
    page.getByRole('textbox', { name: 'Caption for second.png' }),
  ).toHaveValue('Together 🌊')
  await page
    .getByRole('textbox', { name: 'Caption for second.png' })
    .fill('Individual mountain')
  const photos = page.locator('.pending-photo-block')
  await page
    .getByRole('button', { name: 'Drag second.png to reorder' })
    .dragTo(page.getByRole('button', { name: 'Drag first.png to reorder' }))
  await expect(photos.first()).toContainText('second.png')
  await page
    .getByRole('button', { name: 'Close pending shares', exact: true })
    .click()
  await expect(
    page.getByRole('heading', { name: 'Pending shares', exact: true }),
  ).toHaveCount(0)
  await page.getByRole('link', { name: /^Pending shares/ }).click()
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByRole('textbox', { name: 'Shared message', exact: true }),
  ).toHaveValue('Separate source')
  await expect(photos.first()).toContainText('second.png')
  await expect(
    page.getByRole('textbox', { name: 'Caption for second.png' }),
  ).toHaveValue('Individual mountain')
  await page.getByRole('button', { name: 'Move second.png down' }).click()
  await expect(photos.first()).toContainText('first.png')
})

test('navigation during review acquisition releases ownership before an immediate return', async ({
  page,
}) => {
  await page.addInitScript((record) => {
    localStorage.setItem('fixture.strictClaim', 'true')
    localStorage.setItem('fixture.delayClaimOnce', 'true')
    localStorage.setItem('fixture.incoming', JSON.stringify([record]))
  }, record)
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByRole('button', { name: 'Opening share…', exact: true }),
  ).toBeVisible()
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await page.getByRole('link', { name: 'Pending shares', exact: true }).click()
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(
    page.getByRole('textbox', { name: 'Message', exact: true }),
  ).toHaveValue(record.text)
})

test('a busy review shows its failure beside the review button', async ({
  page,
}) => {
  await page.addInitScript(() =>
    localStorage.setItem('fixture.claimBusy', 'true'),
  )
  await page.goto('/e2e/incoming-share.html')
  await page.getByRole('button', { name: 'Review share', exact: true }).click()
  await expect(page.getByRole('alert')).toContainText(
    'Another share operation is active',
  )
  await expect(page.getByRole('alert')).toBeInViewport()
})
