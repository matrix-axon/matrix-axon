import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

for (const thread of [false, true]) {
  test(`completes and sends a custom reaction from the ${thread ? 'thread' : 'room'} command`, async ({
    page,
  }) => {
    const root = {
      account_id: ACCOUNT_ID,
      room_id: ROOM_ID,
      event_id: '$react-root',
      sender: '@alice:hs',
      origin_ts: 1000,
      arrival_order: 1,
      type: 'm.room.message',
      body: 'Reaction fixture',
      content: { msgtype: 'm.text', body: 'Reaction fixture' },
      redacted: false,
      edited: false,
      edit_count: 0,
    }
    const reply = {
      ...root,
      event_id: '$react-reply',
      arrival_order: 2,
      relates_to: { rel_type: 'm.thread', event_id: root.event_id },
    }
    const sent: unknown[] = []
    const requests: string[] = []
    await page.route(
      /\/rooms\/[^/]+\/(?:threads\/[^/]+\/)?timeline(\?|$)/,
      (route) =>
        route.fulfill({
          json: {
            data: {
              events: [
                route.request().url().includes('/threads/') ? reply : root,
              ],
              next_cursor: null,
            },
          },
        }),
    )
    await page.route(/\/events\/[^/]+$/, (route) =>
      route.fulfill({
        json: {
          data: decodeURIComponent(route.request().url()).includes(
            '$react-reply',
          )
            ? reply
            : root,
        },
      }),
    )
    await page.route('**/emoji-packs*', (route) => {
      requests.push(route.request().url())
      expect(
        new URL(route.request().url()).searchParams.get('usage'),
      ).toBeNull()
      return route.fulfill({
        json: {
          data: {
            partial: false,
            packs: [
              {
                room_id: null,
                state_key: null,
                event_type: 'im.ponies.user_emotes',
                display_name: 'Personal',
                images: [
                  {
                    shortcode: 'Adam',
                    url: 'mxc://pack.test/adam',
                    emoticon: true,
                    sticker: true,
                  },
                  {
                    shortcode: 'adam_sticker',
                    url: 'mxc://pack.test/sticker',
                    emoticon: false,
                    sticker: true,
                  },
                ],
              },
              {
                room_id: '!space:hs',
                state_key: 'Inherited',
                event_type: 'm.room.image_pack',
                display_name: 'Inherited',
                images: [
                  {
                    shortcode: 'allthethings',
                    url: 'mxc://pack.test/allthethings',
                    emoticon: true,
                    sticker: false,
                  },
                ],
              },
            ],
          },
        },
      })
    })
    await page.route(/\/events\/[^/]+\/reactions$/, (route) => {
      expect(decodeURIComponent(route.request().url())).toContain(
        thread ? '$react-reply' : '$react-root',
      )
      sent.push(route.request().postDataJSON())
      return route.fulfill({ json: { data: { event_id: '$reaction' } } })
    })
    await page.route(/\/v1\/media\//, (route) =>
      route.fulfill({
        contentType: 'image/png',
        body: Buffer.from(
          'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
          'base64',
        ),
      }),
    )
    await signIn(page)
    await page.goto(thread ? `${ROOM_URL}?thread=%24react-root` : ROOM_URL)
    const composer = page.locator(
      thread ? '.thread-panel .composer' : '.room-stream .composer',
    )
    const draft = composer.locator('textarea')
    await draft.fill('/react AD')
    const choices = composer.getByRole('listbox', { name: 'Emoji matches' })
    const choice = choices.getByRole('option', { name: /:Adam:.*Custom emoji/ })
    await expect(choice).toBeVisible()
    await expect(choice.locator('img')).toBeVisible()
    await expect(
      choices.getByRole('option', { name: /adam_sticker/ }),
    ).toHaveCount(0)
    const bounds = (await choices.boundingBox())!
    const viewport = page.viewportSize()!
    expect(bounds.x).toBeGreaterThanOrEqual(0)
    expect(bounds.y).toBeGreaterThanOrEqual(0)
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width)
    expect(
      await choices.evaluate((el) => el.scrollWidth <= el.clientWidth),
    ).toBe(true)
    await page.screenshot({
      path: test.info().outputPath('react-shortcode.png'),
    })
    await draft.press('Tab')
    await expect(draft).toHaveValue('/react :Adam: ')
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => sent.length).toBe(1)
    expect(sent[0]).toEqual({ key: 'mxc://pack.test/adam', shortcode: 'Adam' })
    await expect(draft).toHaveValue('')
    // An inherited, reaction-only image also resolves without selecting a suggestion.
    await draft.fill('/+ :ALLTHETHINGS:')
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => sent.length).toBe(2)
    expect(sent[1]).toEqual({
      key: 'mxc://pack.test/allthethings',
      shortcode: 'allthethings',
    })
    expect(requests.length).toBeGreaterThanOrEqual(3)
  })
}
