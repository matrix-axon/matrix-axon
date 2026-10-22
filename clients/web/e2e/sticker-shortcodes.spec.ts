import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

for (const thread of [false, true]) {
  test(`a complete shortcode sends a sticker from the ${thread ? 'thread' : 'room'} composer`, async ({
    page,
  }) => {
    const root = {
      account_id: ACCOUNT_ID,
      room_id: ROOM_ID,
      event_id: '$shortcode-root',
      sender: '@alice:hs',
      origin_ts: 1000,
      arrival_order: 1,
      type: 'm.room.message',
      body: 'Shortcode fixture',
      content: { msgtype: 'm.text', body: 'Shortcode fixture' },
      redacted: false,
      edited: false,
      edit_count: 0,
    }
    const sent: unknown[] = []
    const texts: unknown[] = []
    const mediaReads: string[] = []
    let reads = 0
    let lastText = ''
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
    await page.route('**/emoji-packs?*', (route) => {
      reads++
      expect(new URL(route.request().url()).searchParams.get('usage')).toBe(
        'sticker',
      )
      return route.fulfill({
        json: {
          data: {
            packs: [
              {
                room_id: ROOM_ID,
                state_key: 'Testing',
                event_type: 'm.room.image_pack',
                display_name: 'Testing Sticker Pack',
                images: [
                  {
                    shortcode: 'adam',
                    url: 'mxc://pack.test/adam',
                    body: 'Adam sticker',
                    emoticon: false,
                    sticker: true,
                  },
                  {
                    shortcode: `adam_${'x'.repeat(95)}`,
                    url: 'mxc://pack.test/long-adam',
                    sticker: true,
                    emoticon: false,
                  },
                ],
              },
            ],
            partial: false,
          },
        },
      })
    })
    await page.route('**/send-sticker', (route) => {
      sent.push(route.request().postDataJSON())
      return route.fulfill({
        json: { data: { event_id: '$shortcode-sticker' } },
      })
    })
    await page.route('**/send', (route) => {
      texts.push(route.request().postDataJSON())
      lastText = route.request().postDataJSON().body
      return route.fulfill({
        json: { data: { event_id: `$shortcode-text-${texts.length}` } },
      })
    })
    await page.route(/\/events\/[^/]+$/, (route) => {
      const id = decodeURIComponent(
        new URL(route.request().url()).pathname.split('/').at(-1)!,
      )
      const data =
        id === '$shortcode-sticker'
          ? {
              ...root,
              event_id: id,
              arrival_order: 2,
              sender: '@me:hs',
              type: 'm.sticker',
              body: 'Adam sticker',
              content: {
                body: 'Adam sticker',
                url: 'mxc://pack.test/adam',
                info: {},
              },
              relates_to: thread
                ? { rel_type: 'm.thread', event_id: root.event_id }
                : { 'm.in_reply_to': { event_id: root.event_id } },
            }
          : id.startsWith('$shortcode-text-')
            ? {
                ...root,
                event_id: id,
                arrival_order: 3,
                sender: '@me:hs',
                body: lastText,
                content: { msgtype: 'm.text', body: lastText },
                relates_to: thread
                  ? { rel_type: 'm.thread', event_id: root.event_id }
                  : null,
              }
            : root
      return route.fulfill({ json: { data } })
    })
    await page.route(/\/v1\/media\//, (route) => {
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      mediaReads.push(route.request().url())
      return route.fulfill({
        contentType: 'image/png',
        body: Buffer.from(
          'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
          'base64',
        ),
      })
    })
    await signIn(page)
    await page.goto(
      thread
        ? `${ROOM_URL}?thread=${encodeURIComponent(root.event_id)}`
        : ROOM_URL,
    )
    const composer = page.locator(
      thread ? '.thread-panel .composer' : '.room-stream .composer',
    )
    const draft = composer.locator('textarea')
    if (!thread) {
      const row = page.locator('.room-stream [data-event-id="$shortcode-root"]')
      await row.locator('.event-body').click()
      await row.getByRole('button', { name: 'Reply', exact: true }).click()
    }
    await draft.fill(':AdAm:')
    // Typing the closing colon never sends without explicit submission.
    expect(sent).toHaveLength(0)
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => sent.length).toBe(1)
    expect(sent).toEqual([
      {
        body: 'Adam sticker',
        url: 'mxc://pack.test/adam',
        info: null,
        reply_to: thread ? null : root.event_id,
        thread_root: thread ? root.event_id : null,
      },
    ])
    expect(texts).toHaveLength(0)
    await expect(draft).toHaveValue('')
    await draft.fill('Hello :adam:')
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => texts.length).toBe(1)
    expect(texts[0]).toMatchObject({ body: 'Hello :adam:' })
    expect(reads).toBe(1)
    expect(sent).toHaveLength(1)
    await draft.fill(':+1:')
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => texts.length).toBe(2)
    expect(texts[1]).toMatchObject({ body: '👍' })
    expect(reads).toBe(2)
    await draft.fill(':aD')
    const choices = composer.getByRole('listbox', { name: 'Emoji matches' })
    const choice = choices.getByRole('option', { name: /:adam:.*Sticker/ })
    await expect(choice).toBeVisible()
    await expect(choice.locator('img')).toBeVisible()
    await expect
      .poll(() => mediaReads.some((url) => url.includes('/adam/thumbnail?')))
      .toBe(true)
    const viewport = page.viewportSize()!
    const box = (await choices.boundingBox())!
    expect(box.x).toBeGreaterThanOrEqual(0)
    expect(box.y).toBeGreaterThanOrEqual(0)
    expect(box.x + box.width).toBeLessThanOrEqual(viewport.width)
    expect(
      await choices.evaluate((el) => el.scrollWidth <= el.clientWidth),
    ).toBe(true)
    await page.screenshot({
      path: test.info().outputPath('sticker-autocomplete.png'),
    })
    await choice.click()
    await expect(draft).toHaveValue(':adam: ')
    expect(sent).toHaveLength(1)
    await composer.getByRole('button', { name: 'Send', exact: true }).click()
    await expect.poll(() => sent.length).toBe(2)
  })
}

test('failed sticker discovery keeps the draft and send controls visible on a phone', async ({
  page,
}) => {
  await page.setViewportSize({ width: 320, height: 568 })
  await page.route('**/emoji-packs?*', (route) =>
    route.fulfill({
      status: 503,
      json: { error: { code: 'unavailable', message: 'Try later' } },
    }),
  )
  const sent: string[] = []
  await page.route('**/send*', (route) => {
    sent.push(route.request().url())
    return route.fulfill({ status: 500, json: {} })
  })
  await signIn(page)
  await page.goto(ROOM_URL)
  const composer = page.locator('.room-stream .composer')
  const draft = composer.locator('textarea')
  await draft.fill(':adam:')
  await composer.getByRole('button', { name: 'Send', exact: true }).click()
  await expect(composer.getByRole('alert')).toContainText(
    'Your message has not been sent',
  )
  await expect(draft).toHaveValue(':adam:')
  await expect(
    composer.getByRole('button', { name: 'Send', exact: true }),
  ).toBeEnabled()
  const box = (await composer.boundingBox())!
  expect(box.x).toBeGreaterThanOrEqual(0)
  expect(box.x + box.width).toBeLessThanOrEqual(320)
  expect(box.y + box.height).toBeLessThanOrEqual(568)
  expect(sent).toEqual([])
})
