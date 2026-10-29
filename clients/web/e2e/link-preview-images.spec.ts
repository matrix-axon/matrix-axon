import { expect, test, type Locator } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { writeFile } from 'node:fs/promises'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, expectLive, signIn } from './helpers'

// Deterministic 480×270 landscape raster, generated locally for this fixture.
const PNG = readFileSync(
  new URL('./fixtures/link-preview.png', import.meta.url),
)
const encryptedFile = {
  url: 'mxc://preview.test/encrypted',
  v: 'v2',
  key: {
    kty: 'oct',
    alg: 'A256CTR',
    ext: true,
    key_ops: ['encrypt', 'decrypt'],
    k: 'a'.repeat(43),
  },
  iv: 'a'.repeat(22),
  hashes: { sha256: 'a'.repeat(43) },
}

async function decoded(image: Locator) {
  await expect(image).toBeVisible()
  await expect
    .poll(() =>
      image.evaluate(
        (img: HTMLImageElement) => img.complete && img.naturalWidth > 0,
      ),
    )
    .toBe(true)
  await expect(image).toHaveAttribute('src', /^blob:/)
}

for (const thread of [false, true]) {
  test(`bounded plain/encrypted link images keep stable ${thread ? 'thread' : 'room'} geometry`, async ({
    page,
  }, testInfo) => {
    const external: string[] = []
    const media: string[] = []
    let release!: () => void
    const ready = new Promise<void>((resolve) => {
      release = resolve
    })
    await page.route(
      /^https?:\/\/(?:destination|tracking)\.test\//,
      (route) => {
        external.push(route.request().url())
        return route.abort()
      },
    )
    const imageFields = [
      { 'og:image': 'mxc://preview.test/plain' },
      {
        [thread ? 'beeper:image:encryption' : 'matrix:image:encrypted']:
          encryptedFile,
      },
      {},
      { 'og:image': 'https://tracking.test/pixel' },
      {
        'og:image': 'mxc://preview.test/huge',
        'og:image:width': 5000,
        'og:image:height': 3000,
        'matrix:image:size': 3 * 1024 * 1024,
      },
      { 'og:image': 'mxc://preview.test/missing' },
      { 'og:image': 'mxc://preview.test/corrupt' },
      { 'og:image': 'mxc://preview.test/plain', 'matrix:image:encrypted': {} },
    ]
    const urls = imageFields.map((_, i) => `https://destination.test/${i}`)
    const event = (reply: boolean) => ({
      account_id: ACCOUNT_ID,
      room_id: ROOM_ID,
      event_id: reply ? '$image-reply' : '$image-root',
      sender: '@alice:hs',
      origin_ts: Date.now(),
      arrival_order: reply ? 2 : 1,
      type: 'm.room.message',
      state_key: null,
      body: urls.join(' '),
      content: {
        msgtype: reply ? 'm.file' : 'm.text',
        body: urls.join(' '),
        ...(reply
          ? { url: 'mxc://hs/document', filename: 'document.bin' }
          : {}),
        [thread ? 'com.beeper.linkpreviews' : 'm.url_previews']:
          imageFields.map((fields, i) => ({
            [thread ? 'matched_url' : 'matrix:matched_url']: urls[i],
            'og:title': `Article ${i}`,
            'og:description': 'A bundled preview.',
            'og:image:type': 'image/png',
            ...fields,
          })),
      },
      relates_to: reply
        ? { rel_type: 'm.thread', event_id: '$image-root' }
        : null,
      redacted: false,
      edited: false,
      edit_count: 0,
      reactions: null,
    })
    await page.route(
      /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
      (route) =>
        route.fulfill({
          json: {
            data: {
              events: [event(route.request().url().includes('/threads/'))],
              next_cursor: null,
            },
          },
        }),
    )
    await page.route(/\/events\/[^/]+$/, (route) =>
      route.fulfill({ json: { data: event(false) } }),
    )
    await page.route(/\/v1\/media\/[^/]+\/preview\.test\//, async (route) => {
      const url = route.request().url()
      media.push(url)
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      if (thread && url.includes('/plain/thumbnail'))
        return route.fulfill({ status: 404, body: '' })
      if (url.includes('/missing'))
        return route.fulfill({ status: 404, body: '' })
      if (url.includes('/corrupt'))
        return route.fulfill({
          contentType: 'image/png',
          body: PNG.subarray(0, 8),
        })
      await ready
      return route.fulfill({ contentType: 'image/png', body: PNG })
    })
    await signIn(page)
    if (testInfo.project.name === 'chromium')
      await page.setViewportSize({ width: 1400, height: 900 })
    try {
      await page.goto(
        thread
          ? `${ROOM_URL}?thread=${encodeURIComponent('$image-root')}`
          : ROOM_URL,
      )
      const surface = page.locator(
        thread
          ? '.thread-panel [data-event-id="$image-reply"]'
          : '.room-stream [data-event-id="$image-root"]',
      )
      const cards = surface.locator('.link-preview-card')
      const first = cards.nth(0)
      await first.scrollIntoViewIfNeeded()
      const slot = first.locator('.link-preview-image')
      await expect(slot).toBeVisible()
      // The thread fixture rejects the thumbnail before holding its original.
      // Wait for that keyed fallback to mount; a locator handle obtained during
      // the replacement can detach before boundingBox reads it.
      await expect
        .poll(() =>
          media.some((url) =>
            thread ? url.endsWith('/plain') : url.includes('/plain/thumbnail?'),
          ),
        )
        .toBe(true)
      const measured = await first.evaluate((card) => {
        const image = card.querySelector('.link-preview-image')!
        return {
          card: card.getBoundingClientRect().toJSON(),
          slot: image.getBoundingClientRect().toJSON(),
        }
      })
      const before = measured.card
      const slotBefore = measured.slot
      release()
      await decoded(slot.locator('img'))
      const after = await first.boundingBox()
      expect(after!.width).toBeCloseTo(before!.width, 1)
      expect(after!.height).toBeCloseTo(before!.height, 1)
      expect(after!.y).toBeCloseTo(before!.y, 1)
      expect((await slot.boundingBox())!.height).toBeCloseTo(
        slotBefore!.height,
        1,
      )
      expect(slotBefore!.height).toBeLessThanOrEqual(160)
      await cards.nth(1).scrollIntoViewIfNeeded()
      await decoded(cards.nth(1).locator('img'))
      expect(
        media.some((url) =>
          url.includes('/plain/thumbnail?width=480&height=270'),
        ),
      ).toBe(true)
      expect(media.some((url) => url.endsWith('/encrypted'))).toBe(true)
      if (thread) expect(media.some((url) => url.endsWith('/plain'))).toBe(true)
      expect(media.some((url) => url.includes('/encrypted/thumbnail'))).toBe(
        false,
      )
      if (thread)
        await expect(
          page.locator('.thread-root.compact .link-previews'),
        ).toHaveCount(0)
      await surface.locator('summary').click()
      for (let i = 0; i < 8; i++) {
        const card = cards.nth(i)
        await card.scrollIntoViewIfNeeded()
        // The pinned thread root can cover a card the driver considers visible.
        // Center replies explicitly before checking the actual pointer target.
        if (thread)
          await card.evaluate((element) =>
            element.scrollIntoView({ block: 'center' }),
          )
        const bounds = (await card.boundingBox())!
        const viewport = page.viewportSize()!
        expect(bounds.x).toBeGreaterThanOrEqual(0)
        expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width)
        expect(bounds.width).toBeLessThanOrEqual(512)
        expect(
          await card.evaluate((element) => {
            const rect = element.getBoundingClientRect()
            return (
              document
                .elementFromPoint(
                  rect.x + rect.width / 2,
                  rect.y + rect.height / 2,
                )
                ?.closest('.link-preview-card') === element
            )
          }),
        ).toBe(true)
        await expect(card).toHaveAttribute('href', urls[i]!)
        if ([2, 3, 7].includes(i))
          await expect(card.locator('.link-preview-image')).toHaveCount(0)
        if ([5, 6].includes(i)) {
          await expect(card).toContainText('Preview image unavailable')
          await expect(card.locator('img')).toHaveCount(0)
        }
      }
      await cards.nth(4).scrollIntoViewIfNeeded()
      await decoded(cards.nth(4).locator('img'))
      expect(
        media.some((url) =>
          url.includes('/huge/thumbnail?width=480&height=270'),
        ),
      ).toBe(true)
      expect(media.some((url) => url.endsWith('/huge'))).toBe(false)
      // Full thread roots and replies can both own the same image. Each owner
      // gets at most two decode attempts; shared invalidation may refetch it.
      expect(
        media.filter((url) => url.includes('/corrupt')).length,
      ).toBeLessThanOrEqual(thread ? 4 : 2)
      expect(external).toEqual([])
      const geometry = testInfo.outputPath('geometry.json')
      await writeFile(
        geometry,
        JSON.stringify({
          viewport: page.viewportSize(),
          before,
          after,
          slot: slotBefore,
        }),
      )
      await testInfo.attach('geometry', {
        path: geometry,
        contentType: 'application/json',
      })
      await cards.nth(1).scrollIntoViewIfNeeded()
      await testInfo.attach('image-card', {
        body: await page.screenshot({
          path: testInfo.outputPath('image-card.png'),
        }),
        contentType: 'image/png',
      })
    } finally {
      release()
    }
  })
}

for (const recovery of ['button', 'reconnect']) {
  test(`transient preview failures recover by ${recovery} without navigation or layout shift`, async ({
    page,
    request,
  }, testInfo) => {
    let available = false
    const requests: string[] = []
    const url = 'https://destination.test/retry'
    await page.route(/\/rooms\/[^/]+\/timeline(\?|$)/, (route) =>
      route.fulfill({
        json: {
          data: {
            events: [
              {
                account_id: ACCOUNT_ID,
                room_id: ROOM_ID,
                event_id: '$retry-preview',
                sender: '@alice:hs',
                origin_ts: Date.now(),
                arrival_order: 1,
                type: 'm.room.message',
                state_key: null,
                body: url,
                content: {
                  msgtype: 'm.text',
                  body: url,
                  'm.url_previews': [
                    {
                      'matrix:matched_url': url,
                      'og:title': 'Recoverable preview',
                      'og:image': 'mxc://preview.test/retry',
                    },
                  ],
                },
                redacted: false,
                edited: false,
                edit_count: 0,
                relates_to: null,
                reactions: null,
              },
            ],
            next_cursor: null,
          },
        },
      }),
    )
    await page.route(/\/v1\/media\/[^/]+\/preview\.test\/retry/, (route) => {
      requests.push(route.request().url())
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      return available
        ? route.fulfill({ contentType: 'image/png', body: PNG })
        : route.fulfill({ status: 503, body: '' })
    })
    await signIn(page)
    if (testInfo.project.name === 'chromium')
      await page.setViewportSize({ width: 1400, height: 900 })
    await page.goto(ROOM_URL)
    const card = page.locator(
      '[data-event-id="$retry-preview"] .link-preview-card',
    )
    const retry = page.getByRole('button', { name: 'Retry image' })
    await expect(retry).toBeVisible()
    expect(requests).toHaveLength(1)
    const before = (await card.boundingBox())!
    const button = (await retry.boundingBox())!
    const slot = (await card.locator('.link-preview-image').boundingBox())!
    expect(button.x).toBeGreaterThanOrEqual(slot.x)
    expect(button.x + button.width).toBeLessThanOrEqual(slot.x + slot.width)
    expect(button.y).toBeGreaterThanOrEqual(slot.y)
    expect(button.y + button.height).toBeLessThanOrEqual(slot.y + slot.height)
    expect(await retry.evaluate((element) => element.closest('a'))).toBeNull()
    available = true
    if (recovery === 'button') await retry.click()
    else {
      await expectLive(page)
      await request.post('/__e2e/drop-sockets')
    }
    await decoded(card.locator('img'))
    await expect(retry).toHaveCount(0)
    const after = (await card.boundingBox())!
    expect(after.width).toBeCloseTo(before.width, 1)
    expect(after.height).toBeCloseTo(before.height, 1)
    expect(requests).toHaveLength(2)
    expect(
      requests.every((requestUrl) => requestUrl.includes('/thumbnail?')),
    ).toBe(true)
    await expect(page).toHaveURL(new RegExp('/rooms/'))
    await expect(card).toHaveAttribute('href', url)
  })
}
