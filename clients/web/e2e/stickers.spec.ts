import { expect, test } from '@playwright/test'
import {
  ACCOUNT_ID,
  ROOM_ID,
  ROOM_URL,
  signIn,
  expectPaneCenterUncovered,
} from './helpers'

const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
  'base64',
)
const INFO = { w: 240, h: 160, size: PNG.length, mimetype: 'image/png' }
const STICKER = {
  body: 'A friendly wave',
  url: 'mxc://pack.test/wave',
  info: INFO,
}
const root = {
  account_id: ACCOUNT_ID,
  room_id: ROOM_ID,
  event_id: '$sticker-root',
  sender: '@alice:hs',
  origin_ts: 1000,
  arrival_order: 1,
  type: 'm.room.message',
  body: 'Sticker fixture',
  content: { msgtype: 'm.text', body: 'Sticker fixture' },
  redacted: false,
  edited: false,
  edit_count: 0,
}

for (const thread of [false, true]) {
  test(`selects a standalone sticker without submitting the ${thread ? 'thread' : 'room'} draft`, async ({
    page,
  }) => {
    const sent: unknown[] = []
    const originals: string[] = []
    await page.route(
      /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
      (route) =>
        route.fulfill({
          json: {
            data: {
              events: route.request().url().includes('/threads/') ? [] : [root],
              next_cursor: null,
            },
          },
        }),
    )
    const stickerEvent = {
      ...root,
      event_id: '$sent-sticker',
      sender: '@me:hs',
      type: 'm.sticker',
      body: STICKER.body,
      content: STICKER,
      arrival_order: 2,
      relates_to: thread
        ? { rel_type: 'm.thread', event_id: root.event_id }
        : null,
    }
    await page.route(/\/events\/[^/]+$/, (route) =>
      route.fulfill({
        json: {
          data: decodeURIComponent(route.request().url()).endsWith(
            '$sent-sticker',
          )
            ? stickerEvent
            : root,
        },
      }),
    )
    await page.route('**/send-sticker', (route) => {
      sent.push(route.request().postDataJSON())
      return route.fulfill({ json: { data: { event_id: '$sent-sticker' } } })
    })
    await page.route('**/emoji-packs?*', (route) => {
      expect(new URL(route.request().url()).searchParams.get('usage')).toBe(
        'sticker',
      )
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      return route.fulfill({
        json: {
          data: {
            packs: [
              {
                room_id: null,
                state_key: null,
                event_type: 'im.ponies.user_emotes',
                display_name: 'Personal',
                images: [
                  {
                    shortcode: 'personal',
                    url: 'mxc://pack.test/personal',
                    sticker: true,
                    emoticon: false,
                  },
                ],
              },
              {
                room_id: ROOM_ID,
                state_key: 'Testing',
                event_type: 'm.room.image_pack',
                display_name: 'Testing',
                images: [
                  {
                    ...STICKER,
                    shortcode: 'wave',
                    sticker: true,
                    emoticon: false,
                  },
                  {
                    shortcode: 'reaction-only',
                    url: 'mxc://pack.test/emote',
                    emoticon: true,
                    sticker: false,
                  },
                  ...Array.from({ length: 45 }, (_, i) => ({
                    shortcode: `both${i}`,
                    url: `mxc://pack.test/both${i}`,
                    emoticon: true,
                    sticker: true,
                  })),
                ],
              },
            ],
            partial: false,
          },
        },
      })
    })
    await page.route(/\/v1\/media\//, (route) => {
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      if (!route.request().url().includes('/thumbnail?'))
        originals.push(route.request().url())
      return route.fulfill({ contentType: 'image/png', body: PNG })
    })
    await signIn(page)
    await page.goto(
      thread
        ? `${ROOM_URL}?thread=${encodeURIComponent(root.event_id)}`
        : ROOM_URL,
    )
    const pane = page.locator(thread ? '.thread-panel' : '.room-stream')
    const composer = pane.locator('.composer')
    const draft = composer.locator('textarea')
    await draft.fill('Keep this unsent draft')
    const launch = composer.getByRole('button', {
      name: 'Send sticker',
      exact: true,
    })
    await launch.click()
    const dialog = page.getByRole('dialog', { name: 'Sticker picker' })
    await expect(
      dialog.getByRole('heading', { name: 'Personal', exact: true }),
    ).toBeVisible()
    await expect(
      dialog.getByRole('heading', { name: 'Testing', exact: true }),
    ).toBeVisible()
    await expect(
      dialog.getByRole('button', { name: 'Send sticker :reaction-only:' }),
    ).toHaveCount(0)
    const panel = dialog.locator('.sticker-picker-panel')
    const box = (await panel.boundingBox())!
    const viewport = page.viewportSize()!
    expect(box.x).toBeGreaterThanOrEqual(0)
    expect(box.y).toBeGreaterThanOrEqual(0)
    expect(box.x + box.width).toBeLessThanOrEqual(viewport.width)
    expect(box.y + box.height).toBeLessThanOrEqual(viewport.height)
    expect(
      await dialog
        .locator('.custom-emoji-packs')
        .evaluate((el) => el.scrollHeight > el.clientHeight),
    ).toBe(true)
    await page.screenshot({
      path: test.info().outputPath('sticker-picker.png'),
    })
    await dialog
      .getByRole('searchbox', { name: 'Search stickers' })
      .fill('wave')
    await dialog.getByRole('button', { name: 'Send sticker :wave:' }).click()
    await expect(dialog).toHaveCount(0)
    await expect(draft).toHaveValue('Keep this unsent draft')
    await expect(
      pane.locator('[data-event-id="$sent-sticker"] .media-image img'),
    ).toBeVisible()
    expect(sent).toEqual([
      {
        ...STICKER,
        reply_to: null,
        thread_root: thread ? root.event_id : null,
      },
    ])
    await expect
      .poll(() => originals.some((url) => url.includes('/pack.test/wave')))
      .toBe(true)
    await expectPaneCenterUncovered(
      page,
      thread ? '.thread-panel' : '.room-stream',
    )
    await launch.click()
    await expect(dialog.getByRole('searchbox')).toBeFocused()
    await page.keyboard.press('Shift+Tab')
    expect(
      await dialog.evaluate((el) => el.contains(document.activeElement)),
    ).toBe(true)
    await page.keyboard.press('Escape')
    await expect(dialog).toHaveCount(0)
    await expect(launch).toBeFocused()
    await expectPaneCenterUncovered(
      page,
      thread ? '.thread-panel' : '.room-stream',
    )
    expect(sent).toHaveLength(1)
  })
}

test('an older server shows unavailable stickers and leaves the draft intact on a short viewport', async ({
  page,
}) => {
  await page.setViewportSize({ width: 320, height: 367 })
  await page.route('**/emoji-packs?*', (route) =>
    route.fulfill({
      status: 404,
      json: { error: { code: 'not_found', message: 'Unknown route' } },
    }),
  )
  await signIn(page)
  await page.goto(ROOM_URL)
  const composer = page.locator('.room-stream .composer')
  await composer.locator('textarea').fill('Draft')
  await composer
    .getByRole('button', { name: 'Send sticker', exact: true })
    .click()
  const dialog = page.getByRole('dialog', { name: 'Sticker picker' })
  await expect(
    dialog.getByText('Stickers are unavailable for this room or server.'),
  ).toBeVisible()
  const box = (await dialog.locator('.sticker-picker-panel').boundingBox())!
  expect(box.y).toBeGreaterThanOrEqual(0)
  expect(box.y + box.height).toBeLessThanOrEqual(367)
  await dialog.getByRole('button', { name: 'Close picker' }).click()
  await expect(composer.locator('textarea')).toHaveValue('Draft')
  await expectPaneCenterUncovered(page, '.room-stream')
})
