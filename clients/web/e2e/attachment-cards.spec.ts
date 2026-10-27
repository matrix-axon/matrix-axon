import { expect, test, type Locator, type Page } from '@playwright/test'
import { writeFile } from 'node:fs/promises'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

const filename = 'axon-preview-attachment-caption-check.txt'
const caption =
  '[Preview test 2] File with image preview in its caption — https://example.org/axon-preview-check'

async function activate(
  target: Locator,
  touch: boolean,
  position?: { x: number; y: number },
) {
  if (touch) await target.tap({ position })
  else await target.click({ position })
}

async function setup(
  page: Page,
  options: {
    thread?: boolean
    content?: Record<string, unknown>
    mimetype?: string
  } = {},
) {
  const requests: string[] = []
  const event = (reply: boolean) => ({
    account_id: ACCOUNT_ID,
    room_id: ROOM_ID,
    event_id: reply ? '$attachment-card' : '$attachment-root',
    sender: '@alice:hs',
    origin_ts: Date.now(),
    arrival_order: 1,
    type: 'm.room.message',
    state_key: null,
    body: caption,
    content: {
      msgtype: 'm.file',
      body: caption,
      filename,
      url: 'mxc://hs/attachment-card',
      info: { mimetype: options.mimetype ?? 'text/plain', size: 68 },
      ...options.content,
    },
    redacted: false,
    edited: false,
    edit_count: 0,
    relates_to: reply
      ? { rel_type: 'm.thread', event_id: '$attachment-root' }
      : null,
    reactions: null,
  })
  await page.route(
    /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
    (route) =>
      route.fulfill({
        json: {
          data: { events: [event(options.thread === true)], next_cursor: null },
        },
      }),
  )
  await page.route(/\/events\/[^/]+$/, (route) =>
    route.fulfill({ json: { data: event(false) } }),
  )
  await page.route(/\/v1\/media\/[^/]+\/hs\/attachment-card$/, (route) => {
    requests.push(route.request().url())
    expect(route.request().headers().authorization).toBe('Bearer e2e-token')
    return route.fulfill({
      contentType: 'text/plain',
      body: 'Attachment preview contents.\n',
    })
  })
  await signIn(page)
  await page.goto(
    options.thread
      ? `${ROOM_URL}?thread=${encodeURIComponent('$attachment-root')}`
      : ROOM_URL,
  )
  const card = page.locator(
    options.thread
      ? '.thread-panel [data-event-id="$attachment-card"] .media-attachment'
      : '.room-stream [data-event-id="$attachment-root"] .media-attachment',
  )
  await expect(card).toBeVisible()
  return { card, requests }
}

for (const thread of [false, true]) {
  test(`attachment card uses its width and opens a preview from the ${thread ? 'thread' : 'room'} timeline`, async ({
    page,
  }, testInfo) => {
    if (testInfo.project.name === 'chromium')
      await page.setViewportSize({ width: 1400, height: 900 })
    const { card, requests } = await setup(page, { thread })
    const bounds = (await card.boundingBox())!
    const text = (await card
      .locator('.media-attachment-caption')
      .boundingBox())!
    const download = (await card
      .getByRole('button', { name: 'Download', exact: true })
      .boundingBox())!
    expect(text.width).toBeGreaterThan(bounds.width - 40)
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(
      page.viewportSize()!.width,
    )
    expect(download.x + download.width).toBeGreaterThan(
      bounds.x + bounds.width - 20,
    )
    expect(download.y).toBeLessThan(text.y)
    expect(download.width).toBeGreaterThanOrEqual(44)
    expect(download.height).toBeGreaterThanOrEqual(44)
    expect(requests).toEqual([])
    await writeFile(
      testInfo.outputPath('geometry.json'),
      JSON.stringify({
        viewport: page.viewportSize(),
        card: bounds,
        caption: text,
        download,
      }),
    )
    await expect(
      card.getByRole('button', { name: 'Preview', exact: true }),
    ).toHaveCount(0)
    await card.screenshot({ path: testInfo.outputPath('attachment-card.png') })
    await activate(
      card.locator('.media-attachment-caption'),
      testInfo.project.name === 'webkit-iphone',
      { x: 8, y: 8 },
    )
    const dialog = page.getByRole('dialog')
    await expect(dialog).toBeVisible()
    await expect(dialog).toContainText('Attachment preview contents.')
    await expect(
      dialog.getByRole('button', { name: 'Download', exact: true }),
    ).toHaveCSS('color', 'rgb(255, 255, 255)')
    const code = dialog.locator('code').first()
    await activate(code, testInfo.project.name === 'webkit-iphone')
    await expect(dialog).toBeVisible()
    await page.screenshot({
      path: testInfo.outputPath('attachment-preview.png'),
    })
    await page.keyboard.press('Escape')
    await expect(dialog).toBeHidden()
    const opener = card.getByRole('button', { name: `Preview ${filename}` })
    await expect(opener).toBeFocused()
    await page.keyboard.press('Space')
    await expect(dialog).toBeVisible()
    await dialog.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(opener).toBeFocused()
  })
}

test('download and caption links have independent actions', async ({
  page,
  context,
}, testInfo) => {
  const { card, requests } = await setup(page)
  await context.route('https://example.org/**', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: '<title>Linked article</title>',
    }),
  )
  const popupPromise = page.waitForEvent('popup')
  await activate(
    card.getByRole('link', { name: 'https://example.org/axon-preview-check' }),
    testInfo.project.name === 'webkit-iphone',
  )
  const popup = await popupPromise
  await popup.waitForLoadState()
  await popup.close()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(0)
  const saved = page.waitForEvent('download')
  await activate(
    card.getByRole('button', { name: 'Download', exact: true }),
    testInfo.project.name === 'webkit-iphone',
  )
  expect((await saved).suggestedFilename()).toBe(filename)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(1)
})

test('spoilers keep their own action and unsupported files stay download-only', async ({
  page,
}, testInfo) => {
  const { card, requests } = await setup(page, {
    mimetype: 'application/zip',
    content: {
      format: 'org.matrix.custom.html',
      formatted_body: 'Caption <span data-mx-spoiler>hidden caption</span>',
    },
  })
  await activate(
    card.locator('.spoiler'),
    testInfo.project.name === 'webkit-iphone',
  )
  await expect(card.locator('.spoiler')).toHaveClass(/spoiler-revealed/)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(0)
  await expect(card.locator('[aria-haspopup="dialog"]')).toHaveCount(0)
})

test('selecting attachment caption text does not open the preview', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium',
    'Native mouse text selection is a desktop interaction',
  )
  const { card, requests } = await setup(page)
  const positions = await card
    .locator('.media-attachment-caption')
    .evaluate((element) => {
      const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT)
      let text = walker.nextNode()!
      while (text.textContent!.trim().length < 20) text = walker.nextNode()!
      const range = document.createRange()
      range.setStart(text, 0)
      range.setEnd(text, 20)
      const rect = range.getBoundingClientRect()
      return {
        left: rect.left,
        right: rect.right,
        y: rect.top + rect.height / 2,
      }
    })
  await page.mouse.move(positions.left + 1, positions.y)
  await page.mouse.down()
  await page.mouse.move(positions.right - 1, positions.y, { steps: 10 })
  await page.mouse.up()
  expect(
    await page.evaluate(() => window.getSelection()?.toString().length ?? 0),
  ).toBeGreaterThan(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(0)
})
