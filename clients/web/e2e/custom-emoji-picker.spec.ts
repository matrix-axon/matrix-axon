import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
  'base64',
)
const ROOM_MXC = 'mxc://pack.test/room-wave'

function fixtureMessage(reply = false, mine = false) {
  return {
    account_id: ACCOUNT_ID,
    room_id: ROOM_ID,
    event_id: reply ? '$pack-reply' : '$pack-root',
    sender: '@alice:hs',
    origin_ts: 1000,
    arrival_order: reply ? 2 : 1,
    type: 'm.room.message',
    state_key: null,
    body: 'Emoji pack fixture',
    content: { msgtype: 'm.text', body: 'Emoji pack fixture' },
    relates_to: reply ? { rel_type: 'm.thread', event_id: '$pack-root' } : null,
    redacted: false,
    edited: false,
    edit_count: 0,
    reactions: mine
      ? {
          [ROOM_MXC]: {
            shortcode: 'wave',
            count: 1,
            me: true,
            senders: ['@me:hs'],
            my_event_ids: ['$mine'],
          },
        }
      : null,
  }
}

for (const thread of [false, true]) {
  test(`selects custom emoji and withdraws it in the ${thread ? 'thread' : 'room'}`, async ({
    page,
  }) => {
    await page.setViewportSize({ width: 320, height: 568 })
    let mine = false
    let packReads = 0
    const sent: unknown[] = []
    const mediaRequests: string[] = []
    const message = (reply = false) =>
      fixtureMessage(reply, mine && reply === thread)
    await page.route(
      /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
      (route) =>
        route.fulfill({
          json: {
            data: {
              events: [message(route.request().url().includes('/threads/'))],
              next_cursor: null,
            },
          },
        }),
    )
    await page.route(/\/events\/[^/]+$/, (route) => {
      if (route.request().method() === 'DELETE') {
        expect(decodeURIComponent(route.request().url())).toContain('$mine')
        mine = false
        return route.fulfill({ json: { data: {} } })
      }
      const reply =
        decodeURIComponent(
          new URL(route.request().url()).pathname.split('/').at(-1)!,
        ) === '$pack-reply'
      return route.fulfill({ json: { data: message(reply) } })
    })
    await page.route(/\/events\/[^/]+\/reactions$/, (route) => {
      sent.push(route.request().postDataJSON())
      mine = true
      return route.fulfill({ json: { data: { event_id: '$mine' } } })
    })
    await page.route('**/emoji-packs', (route) => {
      packReads++
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      return route.fulfill({
        json: {
          data: {
            partial: false,
            packs: [
              {
                room_id: null,
                event_type: 'im.ponies.user_emotes',
                state_key: null,
                display_name: 'Personal',
                images: [
                  { shortcode: 'wave', url: 'mxc://pack.test/personal-wave' },
                ],
              },
              {
                room_id: ROOM_ID,
                event_type: 'im.ponies.room_emotes',
                state_key: 'Testing',
                display_name: 'Testing',
                images: [
                  { shortcode: 'wave', url: ROOM_MXC },
                  ...Array.from({ length: 40 }, (_, i) => ({
                    shortcode: `emoji${i}`,
                    url: `mxc://pack.test/emoji${i}`,
                  })),
                ],
              },
            ],
          },
        },
      })
    })
    await page.route(/\/v1\/media\//, (route) => {
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      mediaRequests.push(route.request().url())
      return route.fulfill({ contentType: 'image/png', body: PNG })
    })
    await signIn(page)
    await page.goto(
      thread
        ? `${ROOM_URL}?thread=${encodeURIComponent('$pack-root')}`
        : ROOM_URL,
    )
    await page.evaluate(() => {
      document.documentElement.style.setProperty('--safe-top', '62px')
      document.documentElement.style.setProperty('--safe-bottom', '34px')
    })
    const surface = page.locator(
      thread
        ? '.thread-panel [data-event-id="$pack-reply"]'
        : '.room-stream [data-event-id="$pack-root"]',
    )
    await surface.locator('.event-body').click()
    await surface.getByRole('button', { name: 'React', exact: true }).click()
    await page.getByRole('button', { name: 'More reactions' }).click()
    const dialog = page.getByRole('dialog', { name: 'Emoji picker' })
    // A packaged iPhone reports safe areas even when a keyboard shortens the
    // available viewport. Both entry points must keep the category tabs clear.
    await expect
      .poll(async () => (await dialog.boundingBox())!.y)
      .toBeGreaterThanOrEqual(62)
    await dialog.getByRole('button', { name: 'Custom', exact: true }).click()
    await expect(dialog.getByRole('heading', { name: 'Testing' })).toBeVisible()
    if (!thread)
      await page.screenshot({
        path: test.info().outputPath('custom-emoji-picker.png'),
      })
    await expect(
      dialog.getByRole('heading', { name: 'Personal' }),
    ).toBeVisible()
    // Replay the shell measurements after a keyboard reduces and offsets the
    // visual viewport without resizing the layout viewport.
    await page.evaluate(() => {
      document.documentElement.style.setProperty(
        '--app-viewport-height',
        '400px',
      )
      document.documentElement.style.setProperty('--app-viewport-top', '24px')
    })
    await expect
      .poll(async () => {
        const box = (await dialog.boundingBox())!
        return box.y + box.height
      })
      .toBeLessThanOrEqual(390)
    await expect
      .poll(async () => (await dialog.boundingBox())!.y)
      .toBeGreaterThanOrEqual(86)
    const headerControlsUsable = await dialog
      .locator('.reaction-full-picker-head button')
      .evaluateAll((buttons) =>
        buttons.every((button) => {
          const box = button.getBoundingClientRect()
          const hit = document.elementFromPoint(
            box.x + box.width / 2,
            box.y + box.height / 2,
          )
          return hit !== null && button.contains(hit)
        }),
      )
    expect(headerControlsUsable).toBe(true)
    await page.screenshot({
      path: test.info().outputPath('mobile-custom-emoji.png'),
    })
    // The list scrolls inside the popup; its controls stay within every viewport.
    const bounds = await dialog.boundingBox()
    const viewport = page.viewportSize()!
    expect(bounds!.x).toBeGreaterThanOrEqual(0)
    expect(bounds!.y).toBeGreaterThanOrEqual(0)
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width)
    expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height)
    expect(
      await dialog
        .locator('.custom-emoji-packs')
        .evaluate((el) => el.scrollHeight > el.clientHeight),
    ).toBe(true)
    await dialog
      .getByRole('searchbox', { name: 'Search custom emoji' })
      .fill('wave')
    const choices = dialog.getByRole('button', { name: 'React with :wave:' })
    await expect(choices).toHaveCount(2)
    await expect(choices.last().locator('img')).toBeVisible()
    await expect
      .poll(() => mediaRequests.some((url) => url.includes('/room-wave')))
      .toBe(true)
    await choices.last().focus()
    await page.keyboard.press('Enter')
    await expect(dialog).toHaveCount(0)
    expect(sent).toEqual([{ key: ROOM_MXC, shortcode: 'wave' }])
    const chip = surface.getByRole('button', {
      name: 'wave: 1, reacted',
      exact: true,
    })
    await expect(chip).toBeVisible()
    await chip.click()
    await expect(surface.locator('.reaction-chip')).toHaveCount(0)
    // Reopening obtains a fresh snapshot and Escape dismisses the portal.
    await surface.locator('.event-body').click()
    await surface.getByRole('button', { name: 'React', exact: true }).click()
    await page.getByRole('button', { name: 'More reactions' }).click()
    await dialog.getByRole('button', { name: 'Custom', exact: true }).click()
    await expect.poll(() => packReads).toBe(2)
    await page.keyboard.press('Escape')
    await expect(dialog).toHaveCount(0)
  })
}

test('an older server leaves Unicode reactions usable', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 568 })
  await page.route('**/emoji-packs', (route) =>
    route.fulfill({
      status: 404,
      json: { error: { code: 'not_found', message: 'Unknown route' } },
    }),
  )
  await page.route(/\/rooms\/[^/]+\/timeline(\?|$)/, (route) =>
    route.fulfill({
      json: { data: { events: [fixtureMessage()], next_cursor: null } },
    }),
  )
  await signIn(page)
  await page.goto(ROOM_URL)
  const row = page.locator('.room-stream [data-event-id="$pack-root"]')
  await row.locator('.event-body').click()
  await row.getByRole('button', { name: 'React', exact: true }).click()
  await page.getByRole('button', { name: 'More reactions' }).click()
  const dialog = page.getByRole('dialog', { name: 'Emoji picker' })
  await dialog.getByRole('button', { name: 'Custom', exact: true }).click()
  await expect(
    dialog.getByText('Custom emoji are unavailable for this room or server.'),
  ).toBeVisible()
  const headerFits = await dialog
    .locator('.reaction-full-picker-head')
    .evaluate((header) => {
      const bounds = header.getBoundingClientRect()
      return Array.from(header.querySelectorAll('button')).every((button) => {
        const box = button.getBoundingClientRect()
        return box.left >= bounds.left && box.right <= bounds.right
      })
    })
  expect(headerFits).toBe(true)
  await dialog.getByRole('button', { name: 'Emoji', exact: true }).click()
  await expect(dialog.locator('emoji-picker')).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
})
