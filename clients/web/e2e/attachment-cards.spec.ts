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
    msgtype?: string
    doubleTap?: 'reply' | null
  } = {},
) {
  const requests: string[] = []
  if (options.doubleTap !== undefined) {
    await page.route('**/v1/preferences/message_gestures', (route) =>
      route.fulfill({
        json: {
          data: {
            key: 'message_gestures',
            value: {
              schema_version: 1,
              bindings: {
                double_tap: options.doubleTap,
                touch_and_hold: 'thread',
                swipe_left: options.doubleTap === 'reply' ? 'react' : 'reply',
              },
              reaction_emoji: '👍',
            },
            updated_at: new Date().toISOString(),
          },
        },
      }),
    )
  }
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
      msgtype: options.msgtype ?? 'm.file',
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
  const card = page
    .locator(
      options.thread
        ? '.thread-panel [data-event-id="$attachment-card"]'
        : '.room-stream [data-event-id="$attachment-root"]',
    )
    .locator('.media-attachment, .media-tile')
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
      .getByRole('button', { name: `Download ${filename}`, exact: true })
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
    await expect(card.locator('xpath=ancestor::li')).not.toHaveClass(
      /actions-open/,
    )
    await expect(dialog).toContainText('Attachment preview contents.')
    await expect(
      dialog.getByRole('button', { name: `Download ${filename}`, exact: true }),
    ).toHaveCSS('color', 'rgb(255, 255, 255)')
    const code = dialog.locator('code').first()
    await activate(code, testInfo.project.name === 'webkit-iphone')
    await expect(dialog).toBeVisible()
    // Use the viewer interior: Firefox hits the backdrop at its rounded corner.
    await activate(
      dialog.locator('.media-text-preview'),
      testInfo.project.name === 'webkit-iphone',
    )
    await expect(dialog).toBeVisible()
    await page.screenshot({
      path: testInfo.outputPath('attachment-preview.png'),
    })
    await page.keyboard.press('Escape')
    await expect(dialog).toBeHidden()
    const opener = card.getByRole('button', { name: filename, exact: true })
    await expect(opener).toBeFocused()
    await page.keyboard.press('Space')
    await expect(dialog).toBeVisible()
    await dialog.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(opener).toBeFocused()
  })
}

test('a single tap opens the card with double-tap actions disabled', async ({
  page,
}, testInfo) => {
  const { card } = await setup(page, { doubleTap: null })
  await expect(card.locator('xpath=ancestor::li')).toHaveClass(
    /touch-hold-enabled/,
  )
  const events: string[] = []
  page.on('console', (message) => {
    if (message.text().startsWith('attachment-input'))
      events.push(message.text())
  })
  await page.evaluate(() => {
    for (const type of ['pointerdown', 'pointerup', 'click']) {
      document.addEventListener(
        type,
        (event) => {
          const target = event.target as Element
          console.debug(
            'attachment-input',
            event.type,
            target.tagName,
            target.className,
          )
        },
        true,
      )
    }
  })
  await activate(
    card.locator('.media-attachment-caption'),
    testInfo.project.name === 'webkit-iphone',
    { x: 8, y: 8 },
  )
  await page.waitForTimeout(400)
  await testInfo.attach('pointer-events', {
    body: JSON.stringify(events),
    contentType: 'application/json',
  })
  await expect(page.getByRole('dialog')).toBeVisible()
})

test('a configured double tap takes priority over opening the preview', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name !== 'webkit-iphone',
    'Native touch double-tap uses the iPhone profile',
  )
  const { card, requests } = await setup(page, { doubleTap: 'reply' })
  await expect(card.locator('xpath=ancestor::li')).toHaveClass(
    /touch-hold-enabled/,
  )
  const target = card.getByRole('button', { name: filename, exact: true })
  await target.tap()
  await target.tap()
  await expect(page.getByText('Replying to')).toBeVisible()
  await page.waitForTimeout(400)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(0)
})

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
    card.getByRole('button', { name: `Download ${filename}`, exact: true }),
    testInfo.project.name === 'webkit-iphone',
  )
  expect((await saved).suggestedFilename()).toBe(filename)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(1)
})

for (const [kind, mimetype] of [
  ['file', 'text/plain'],
  ['audio', 'audio/mpeg'],
  ['video', 'video/mp4'],
  ['file', 'application/pdf'],
]) {
  test(`attachment gestures preserve swipe and hold for ${mimetype}`, async ({
    page,
  }) => {
    const { card, requests } = await setup(page, {
      msgtype: `m.${kind}`,
      mimetype,
    })
    const target = card
      .locator('.media-tile-surface, .media-attachment-caption')
      .first()
    const pointer = async (type: string, x: number) => {
      await target.dispatchEvent(type, {
        bubbles: true,
        cancelable: true,
        pointerType: 'touch',
        pointerId: 1,
        isPrimary: true,
        clientX: x,
        clientY: 300,
      })
    }
    await pointer('pointerdown', 260)
    await pointer('pointermove', 210)
    await expect(card.locator('xpath=ancestor::li')).toHaveClass(
      /gesture-swipe-reveal/,
    )
    await pointer('pointermove', 140)
    await pointer('pointerup', 140)
    await target.dispatchEvent('click', {
      bubbles: true,
      cancelable: true,
      detail: 1,
    })
    await expect(page.getByText('Replying to')).toBeVisible()
    await expect(page.getByRole('dialog')).toHaveCount(0)
    await pointer('pointerdown', 260)
    await page.waitForTimeout(650)
    await pointer('pointerup', 260)
    await target.dispatchEvent('click', {
      bubbles: true,
      cancelable: true,
      detail: 1,
    })
    await expect(
      page.getByRole('complementary', { name: 'Thread' }),
    ).toBeVisible()
    await expect(page.getByRole('dialog')).toHaveCount(0)
    expect(requests).toHaveLength(0)
  })
}

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
  // A row click can defer opening its action bar for the double-click window.
  await page.waitForTimeout(400)
  await expect(card.locator('xpath=ancestor::li')).not.toHaveClass(
    /actions-open/,
  )
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(requests).toHaveLength(0)
})
