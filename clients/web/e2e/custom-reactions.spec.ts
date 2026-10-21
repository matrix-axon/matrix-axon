import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, ROOM_URL, signIn } from './helpers'

const MXC = 'mxc://reaction.test/custom'
const MISSING = 'mxc://reaction.test/missing'
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQAAAAA3bvkkAAAACklEQVQI12NoAAAAggCB3UNq9AAAAABJRU5ErkJggg==',
  'base64',
)

for (const thread of [false, true]) {
  test(`custom reaction images load and toggle in the ${thread ? 'thread' : 'room'} timeline`, async ({
    page,
  }) => {
    let mine = false
    const sent: unknown[] = []
    const requests: string[] = []
    let releaseImage!: () => void
    const imageReady = new Promise<void>((resolve) => {
      releaseImage = resolve
    })
    const event = (reply = false) => ({
      account_id: ACCOUNT_ID,
      room_id: ROOM_ID,
      event_id: reply ? '$custom-reply' : '$custom-root',
      sender: '@alice:hs',
      origin_ts: Date.now(),
      arrival_order: reply ? 2 : 1,
      type: 'm.room.message',
      state_key: null,
      body: 'Custom reaction fixture',
      content: { msgtype: 'm.text', body: 'Custom reaction fixture' },
      relates_to: reply
        ? { rel_type: 'm.thread', event_id: '$custom-root' }
        : null,
      redacted: false,
      edited: false,
      edit_count: 0,
      reactions: {
        [MXC]: {
          count: mine ? 2 : 1,
          me: mine,
          senders: mine ? ['@alice:hs', '@me:hs'] : ['@alice:hs'],
          my_event_ids: mine ? ['$my-custom-reaction'] : [],
        },
        [MISSING]: {
          count: 1,
          me: false,
          senders: ['@alice:hs'],
          my_event_ids: [],
        },
        '👍': { count: 3, me: false, senders: ['@alice:hs'], my_event_ids: [] },
      },
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
    await page.route(/\/events\/[^/]+$/, async (route) => {
      if (route.request().method() === 'DELETE') {
        expect(decodeURIComponent(route.request().url())).toContain(
          '$my-custom-reaction',
        )
        mine = false
        return route.fulfill({ json: { data: {} } })
      }
      return route.fulfill({ json: { data: event(thread) } })
    })
    await page.route(/\/events\/[^/]+\/reactions$/, async (route) => {
      sent.push(route.request().postDataJSON())
      mine = true
      return route.fulfill({
        json: { data: { event_id: '$my-custom-reaction' } },
      })
    })
    await page.route(/\/v1\/media\//, async (route) => {
      requests.push(route.request().url())
      expect(route.request().headers().authorization).toBe('Bearer e2e-token')
      if (route.request().url().includes('/missing')) {
        return route.fulfill({
          status: 404,
          json: { error: { code: 'not_found', message: 'Missing' } },
        })
      }
      await imageReady
      return route.fulfill({ contentType: 'image/png', body: PNG })
    })
    await signIn(page)
    try {
      await page.goto(
        thread
          ? `${ROOM_URL}?thread=${encodeURIComponent('$custom-root')}`
          : ROOM_URL,
      )
      const surface = page.locator(
        thread
          ? '.thread-panel [data-event-id="$custom-reply"]'
          : '.room-stream [data-event-id="$custom-root"]',
      )
      const chip = surface.locator('.reaction-chip').first()
      const slot = chip.locator('.reaction-image')
      await expect(slot).toBeVisible()
      const before = await chip.boundingBox()
      releaseImage()
      await expect(slot.locator('img')).toBeVisible()
      await expect
        .poll(() =>
          slot
            .locator('img')
            .evaluate(
              (img: HTMLImageElement) => img.complete && img.naturalWidth > 0,
            ),
        )
        .toBe(true)
      const after = await chip.boundingBox()
      expect(after!.width).toBeCloseTo(before!.width, 1)
      expect(after!.height).toBeCloseTo(before!.height, 1)
      const bounds = await slot.boundingBox()
      expect(bounds!.width).toBeCloseTo(20, 2)
      expect(bounds!.height).toBeCloseTo(20, 2)
      const viewport = page.viewportSize()!
      expect(after!.x).toBeGreaterThanOrEqual(0)
      expect(after!.x + after!.width).toBeLessThanOrEqual(viewport.width)
      await expect(chip).toHaveAccessibleName('Custom reaction: 1')
      await chip.click()
      await expect(chip).toHaveAccessibleName('Custom reaction: 2, reacted')
      expect(sent).toEqual([{ key: MXC }])
      await chip.click()
      await expect(chip).toHaveAccessibleName('Custom reaction: 1')
      const missing = surface.locator('.reaction-chip').nth(1)
      await expect(missing.locator('img')).toHaveCount(0)
      await expect(missing).toHaveText('◇ 1')
      await expect(
        surface.getByRole('button', { name: '👍 3', exact: true }),
      ).toBeVisible()
      expect(
        requests.every((url) =>
          url.includes(`/v1/media/${ACCOUNT_ID}/reaction.test/`),
        ),
      ).toBe(true)
      expect(requests.some((url) => url.includes('/thumbnail?'))).toBe(true)
    } finally {
      releaseImage()
    }
  })
}

// A clicked chip retains focus while the pointer moves to another reaction.
// Tooltips share the message column's anchor, so two open tooltips overlap.
test('hovering custom reactions replaces the tooltip of a focused sibling', async ({
  page,
}) => {
  const reactions = Object.fromEntries(
    ['alice', 'bob', 'carol'].map((name) => [
      `mxc://reaction.test/${name}`,
      { count: 1, me: false, senders: [`@${name}:hs`], my_event_ids: [] },
    ]),
  )
  await page.route('**/timeline*', (route) =>
    route.fulfill({
      json: {
        data: {
          events: [
            {
              account_id: ACCOUNT_ID,
              room_id: ROOM_ID,
              event_id: '$hover-reactions',
              sender: '@alice:hs',
              origin_ts: 1000,
              arrival_order: 1,
              type: 'm.room.message',
              state_key: null,
              body: 'Reaction tooltip fixture',
              content: { msgtype: 'm.text', body: 'Reaction tooltip fixture' },
              relates_to: null,
              redacted: false,
              edited: false,
              edit_count: 0,
              reactions,
            },
          ],
          next_cursor: null,
        },
      },
    }),
  )
  await page.route(/\/v1\/media\//, (route) =>
    route.fulfill({ contentType: 'image/png', body: PNG }),
  )
  await signIn(page)
  await page.goto(ROOM_URL)
  const row = page.locator('[data-event-id="$hover-reactions"]')
  const chips = row.locator('.reaction-chip')
  await expect(chips).toHaveCount(3)
  // Focus the last chip, as happens after clicking it, without sending a react.
  await chips.nth(2).focus()
  await expect(row.getByRole('tooltip')).toContainText('@carol:hs')
  for (const index of [0, 1, 2, 0]) {
    await chips.nth(index).hover()
    await expect(row.getByRole('tooltip')).toHaveCount(1)
    await expect(row.getByRole('tooltip')).toHaveText(
      `Custom reaction: @${['alice', 'bob', 'carol'][index]}:hs`,
    )
  }
})
