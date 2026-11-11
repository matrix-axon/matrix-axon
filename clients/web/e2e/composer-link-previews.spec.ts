import { expect, test, type Page } from '@playwright/test'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { ROOM_URL, expectLive, signIn } from './helpers'

/**
 * Composer link previews in a browser (ADR 0132): no device source, so a
 * card offers the homeserver, which is asked only from its button. Covers
 * what jsdom cannot: the thumbnail bytes on the upload, the card's image,
 * and the strip's fit at phone width.
 */

// Deterministic 480×270 raster, the same fixture as the timeline cards.
const PNG = readFileSync(
  new URL('./fixtures/link-preview.png', import.meta.url),
)
const LINK = 'https://news.example.org/story'
const THUMBNAIL_ID = '44444444-4444-4444-8444-444444444444'

/**
 * What the mock server staged. Its record, not a route handler's: Playwright
 * hands a WebKit route an empty body for a `File` upload (measured: 0 bytes
 * under webkit-desktop, all 1,008 under chromium).
 */
async function staged(page: Page) {
  const res = await page.request.get('/__e2e/uploads')
  return (
    (await res.json()) as {
      data: { kind: string; content_type: string | null; sha1: string }[]
    }
  ).data
}

async function routePreviews(page: Page) {
  const generated: unknown[] = []
  const sent: Record<string, unknown>[] = []
  const external: string[] = []
  await page.context().route(/^https?:\/\/news\.example\.org\//, (route) => {
    external.push(route.request().url())
    return route.abort()
  })
  await page.addInitScript(() =>
    localStorage.setItem(
      'axon.settings',
      JSON.stringify({ version: 1, linkPreviews: 'device+homeserver' }),
    ),
  )
  await page.route(/\/link-previews\/generate$/, (route) => {
    generated.push(route.request().postDataJSON())
    return route.fulfill({
      json: {
        data: {
          status: 'ready',
          thumbnail_status: 'ready',
          image_expires_at: null,
          preview: {
            matched_url: LINK,
            title: 'A story with a long title that should stay on its card',
            description: 'What the homeserver read from the page.',
            site_name: 'Example News',
            image_upload_id: THUMBNAIL_ID,
          },
        },
      },
    })
  })
  await page.route(
    new RegExp(`/media/uploads/${THUMBNAIL_ID}/preview$`),
    (route) => route.fulfill({ contentType: 'image/png', body: PNG }),
  )
  await page.route(/\/rooms\/[^/]+\/send$/, (route) => {
    sent.push(route.request().postDataJSON() as Record<string, unknown>)
    return route.fulfill({
      json: { data: { event_id: `$sent-${sent.length}` } },
    })
  })
  return { generated, sent, external }
}

test('a browser card asks the homeserver on request, and sends its thumbnail', async ({
  page,
}) => {
  const seen = await routePreviews(page)
  await signIn(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(ROOM_URL)
  await expectLive(page)
  // The mock server outlives one run, so count from here.
  const before = (await staged(page)).length
  const field = page.getByRole('textbox', { name: /^Message/ })
  await field.fill(`read this ${LINK} `)

  const strip = page.getByRole('list', { name: 'Link previews to send' })
  const card = strip.getByRole('listitem')
  await expect(card).toContainText('news.example.org')
  await expect(card).toContainText('It will see the link')
  // Nothing is asked until the user does.
  await page.waitForTimeout(600)
  expect(seen.generated).toEqual([])

  await card.getByRole('button', { name: 'Try homeserver' }).click()
  await expect(card).toContainText('Example News')
  const image = card.locator('img')
  await expect
    .poll(() =>
      image.evaluate(
        (img: HTMLImageElement) => img.complete && img.naturalWidth > 0,
      ),
    )
    .toBe(true)
  expect(seen.generated).toEqual([{ url: LINK }])

  // The strip fits a phone: the card stays inside the viewport and the page
  // gains no sideways scroll.
  const box = (await card.boundingBox())!
  expect(box.x).toBeGreaterThanOrEqual(0)
  expect(box.x + box.width).toBeLessThanOrEqual(390)
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true)
  const thumbnail = (await image.boundingBox())!
  expect(thumbnail.height).toBeLessThanOrEqual(box.height)

  // At phone width Enter is a new line; Send sends.
  await page.getByRole('button', { name: 'Send', exact: true }).click()
  await expect.poll(() => seen.sent.length).toBe(1)
  const [preview] = seen.sent[0].url_previews as Record<string, unknown>[]
  expect(preview).toEqual({
    matched_url: LINK,
    title: 'A story with a long title that should stay on its card',
    description: 'What the homeserver read from the page.',
    site_name: 'Example News',
    image_upload_id: expect.any(String),
  })
  // A fresh upload of the bytes read back, never the generated handle.
  expect(preview.image_upload_id).not.toBe(THUMBNAIL_ID)
  const uploads = (await staged(page)).slice(before)
  expect(uploads).toHaveLength(1)
  expect(uploads[0]).toMatchObject({
    kind: 'image',
    content_type: 'image/png',
    sha1: createHash('sha1').update(PNG).digest('hex'),
  })
  await expect(strip).toBeHidden()
  expect(seen.external).toEqual([])
})

test('a discarded card sends the "no previews" marker', async ({ page }) => {
  const seen = await routePreviews(page)
  await signIn(page)
  await page.goto(ROOM_URL)
  await expectLive(page)
  const field = page.getByRole('textbox', { name: /^Message/ })
  await field.fill(`read this ${LINK} `)
  await page
    .getByRole('button', { name: 'Remove the preview of news.example.org' })
    .click()
  await expect(
    page.getByRole('list', { name: 'Link previews to send' }),
  ).toBeHidden()
  await page.getByRole('button', { name: 'Send', exact: true }).click()
  await expect.poll(() => seen.sent.length).toBe(1)
  expect(seen.sent[0].url_previews).toEqual([])
  expect(seen.generated).toEqual([])
  expect(seen.external).toEqual([])
})
